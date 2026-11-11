# Remote assistance with clud.bot

`clud clud.bot` is an exact shortcut for `clud remote connect clud.bot`,
including the same accepted flags and argument validation. It dispatches directly
to the remote-connect implementation with the host fixed to `clud.bot`.
`clud clud.bot --help` displays command help without opening a session.

`clud remote connect clud.bot` creates a foreground operator session, prints a
secret keyword and the browser status URL, and waits for operator acceptance.
The user shares the keyword with their operator over an existing trusted channel.
A hostname means HTTPS. An explicit HTTP origin is accepted only on loopback
for development. Returned WebSocket credentials are accepted only for the same
origin, never printed or saved, and WebSocket redirects are disabled.

Acceptance does not grant control. A control request prints
`Allow Remote Control by Zach Vorhies? [y/N]` in the client's terminal. Only a
local affirmative answer followed by the matching grant opens access to a shell.
A negative answer denies control. The status page cannot grant consent.
The client advertises PTY support and uses running-process's native PTY adapter;
no system SSH daemon, persistent key or separate helper executable is needed.
The shell runs as the current user in the client's current directory. The
operator can read and modify anything that user can access. Only connect to an
operator you trust. Closing the session stops the shell; this does not undo its
commands or revoke credentials that an executed command copied elsewhere.

Client output is binary channel 0; server input is channel 1. JSON messages
carry capability negotiation, consent, terminal dimensions, exit status and
session state. Heartbeats run every 20 seconds. Control revocation clears local
approval and closes the PTY. Disconnect, denial, expiration and Ctrl+C terminate
the foreground client and close the PTY through its cleanup guard. Client output
and control events are not written to files by clud.

The shell's output also goes to the client's own terminal, so the person who
consented can watch what the operator does (`remote_connect/console.rs`). Bytes
are mirrored unchanged except terminal queries, which the operator's terminal
answers: a local reply would land on stdin, on screen and in a consent answer.
Only the queries `terminal_queries` knows are removed. The PTY keeps the
operator's dimensions, so a differently sized local terminal can wrap a
full-screen program oddly. When the shell ends or control is revoked the client
undoes what the program left on (alternate screen, input modes, keyboard frames)
before printing anything else. A single worker owns stdout: past 2 MiB waiting
on a stalled local terminal, mirrored output is dropped and the gap is reported,
while the session and the server's copy carry on unaffected.

ConPTY holds a new shell until its first `ESC[6n` is answered. The Windows client
answers that one query and removes it from the stream: the operator's terminal may
not be attached or hold control, and a second reply would reach the shell as input.
ConPTY also keeps its output open after the shell exits, so the pump polls for exit
whenever output is idle rather than waiting for the reader's EOF.

Terminal writes run on a separate bounded worker so a program that stops reading
input cannot prevent session cancellation. Output frames are at most 16 KiB and
the forwarding queue holds at most 128 frames (2 MiB). An overloaded input/output pump
ends the session with an explicit error and closes the shell. This bounds noisy
or blocked terminals; a sufficiently fast output burst can hit the limit.

The server contract is maintained in `talos-hq/clud.bot`'s `SPEC.md`. This native
client advertises `pty`, not the legacy exec capability. The session creation
response supplies a short-lived client bearer and proxy model. The native client
validates that the bearer matches its same-origin WebSocket credential and derives
the inference URL from the user-selected origin, never from an arbitrary returned
URL. Its remote HTTP and WebSocket connectors combine bundled public roots with
system trust; unrelated HTTP clients keep their existing trust policy.

## Choosing a mode: `clud connect`

`clud connect clud.bot` asks first:

```text
1. (*) Allow Full Control
2. ( ) Use Proxy

Make your selection:
```

Enter takes the marked row, the arrows move it, and `1` or `2` choose directly.
Without a terminal it reads one typed line; closed stdin selects nothing.
**Allow Full Control** is the session described above, including the per-request
`[y/N]`: the choice names what may be asked for, and does not answer the request.

**Use Proxy** creates a session with `"mode":"proxy"`, for which the server starts
inference off. The operator's admin page shows the request, naming the server's
model routed through OpenRouter (DeepSeek by default), and approving it is what
turns inference on. The operator gets no shell: the server answers `control`
and `key-request` with 409 `proxy_only`, and this client advertises only `proxy`
and has no handler for control or PTY events. A server that does not echo the
mode cannot ask for that consent, so the client stops rather than connect.

On approval the client starts `clud --claude --safe` in the user's terminal, with
the environment in [Ephemeral inference](#ephemeral-inference). `--safe` is required:
every reply comes through the operator's server, so a tool call must stop at the
user's own permission prompt. Even so, the server's replies can propose commands,
so this mode still means trusting the operator. Ctrl+C then belongs to
the agent. Ending the proxy or losing the server does not kill the agent, since
it is the user's own work; its requests are refused instead. The client exits
with the agent's code.

`--model` is reserved: it parses, and is refused with exit code 2 before any prompt
or network call. The server forces every request in a proxy session to the model
the operator approved.

## Ephemeral inference

The consented shell receives `ANTHROPIC_BASE_URL=<origin>/api/proxy`,
`ANTHROPIC_AUTH_TOKEN=<session bearer>`, and the corresponding OpenAI-compatible
`OPENAI_BASE_URL=<origin>/api/proxy/v1` and `OPENAI_API_KEY`. Existing provider keys,
OAuth tokens and auth-header overrides are removed from that child's environment.
The bearer is an ephemeral server capability, never an OpenRouter/provider key.
The real OpenRouter credential remains on clud.bot.

Claude and Codex configuration paths point into a session-owned temporary
configuration directory; no credential is written there. It holds only a seeded
Claude `.claude.json` marking onboarding complete, and clud's launch-scope picker
stays out of the way. Claude's per-folder trust check still appears: it is a safety
decision, not a credential. A missing Claude Code still prompts to install it. A nested `clud -p`
launch recognizes the private `CLUD_REMOTE_PROXY_ORIGIN`,
`CLUD_REMOTE_PROXY_TOKEN` and `CLUD_REMOTE_PROXY_MODEL` environment context before
credential-based provider selection. It uses the direct Claude wire protocol,
bypasses local credential-store construction, disables user/project setting
sources and status-line settings discovery, and refuses alternate providers,
background launches and explicit settings overrides. All normal launches still
use the canonical LaunchPlan and runner. The remote route never falls back to a
local provider if the proxy refuses a request.

### Model requests ride the session socket

clud.bot's hosting edge runs a request firewall over HTTPS bodies. An agent's
prompt carries shell commands and tool output, and the edge answers some of those
requests with its own HTML `403 Blocked` page, which Claude reports as "Please
run /login · API Error: 403 Blocked". The request never reaches the server, so it
looks like a dead session. WebSocket frames are not inspected.

When the server's `hello` carries `"proxy_channel": 2`, the client
(`remote_connect/bridge.rs`) opens a loopback HTTP endpoint and points
`ANTHROPIC_BASE_URL`/`OPENAI_BASE_URL` at it instead of the origin. It also exports
`CLUD_REMOTE_PROXY_BRIDGE` so a nested launch rebuilds the same environment. The
endpoint serves only callers presenting the session bearer, and only under
`/api/proxy/v1/`. Each request is relayed as channel-2 binary frames on the
socket the session already holds, and the server's status, content type and
streamed body are written back unchanged. A server without `proxy_channel`
keeps the direct HTTPS routes. The frame layout is owned by the server's
`SPEC.md` §4.3.1.

Proxy disablement leaves terminal control intact but invalidates inference on
the server. Disconnect, denial and session end revoke the bearer and close the
shell. The configuration-directory guard lives until the foreground remote
session ends. A key request still receives an explicit unsuccessful response:
this command never persists an OpenAI key.

## Validation

`cli::remote_connect_e2e_cli` runs the actual client and a real shell against an
in-process operator server on every host, without clud.bot or credentials.
The CLI parsing regression is `remote_connect::tests`. Transport unit tests
reject insecure origins and cross-origin bearer handoff. The site's
`tests/e2e/native_client.py` launches the actual built Rust executable against
its FastAPI app over real HTTP and WebSockets. It checks keyword creation,
operator acceptance, denial of consent, affirmative consent, real shell output,
disconnect and operator denial:

```sh
# In the client repository:
SOLDR_PORCELAIN=1 soldr cargo test -p clud --lib remote_connect
SOLDR_PORCELAIN=1 soldr cargo build -p clud --bin clud
# In the clud.bot repository, pass the absolute executable path:
CLUD_TEST_BINARY=/path/to/clud/target/debug/clud uv run pytest -q tests/e2e/native_client.py
```

A build older than this command does not reject it: unknown words pass through
to the harness, so `clud remote connect` opens Claude Code and its login. The
installer uses the latest GitHub release, so the command works only once a release
containing it is published. Update clud and start a new
terminal. `clud remote connect --help` checks the executable on PATH before a
session is created.


The expensive two-container Bosn proof is disabled by default in the server
repository. See `talos-hq/clud.bot/tests/e2e/ephemeral_proxy/README.md` and its
`AGENTS.md` for explicit enablement, server-only key provisioning, credential
access tracing, controlled-vs-live upstream evidence and cleanup. The lasting
client coverage is `remote_connect::proxy::tests` plus
`cli::remote_proxy_cli` (native clud and a real HTTP probe from mock-agent).
The server's ordinary API suite checks native Messages streaming/tool passthrough,
server-only upstream authorization, token accounting and revocation.

# Remote assistance with clud.bot

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

Terminal writes run on a separate bounded worker so a program that stops reading
input cannot prevent session cancellation. Output frames are at most 16 KiB and
the forwarding queue holds at most 128 frames (2 MiB). An overloaded input/output pump
ends the session with an explicit error and closes the shell. This bounds noisy
or blocked terminals; a sufficiently fast output burst can hit the limit.

The server contract is maintained in `talos-hq/clud.bot`'s `SPEC.md`. This native
client advertises `pty`, not the legacy `clud -p` exec capability. Model proxy
configuration and local OpenAI-key storage are not enabled by this command; a
key request receives an explicit unsuccessful response.

## Validation

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

When an older installation rejects `connect`, update clud and start a new
terminal. `clud remote connect --help` checks the executable on PATH before a
session is created.

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
`Allow Remote Control by Zach Vorhies? [y/N]:` in the client's terminal, with the cursor left on that line for the answer. Only a
local affirmative answer followed by the matching grant opens access to a shell.
A negative answer denies control. The status page cannot grant consent.
The client advertises PTY support and uses running-process's native PTY adapter;
no system SSH daemon, persistent key or separate helper executable is needed
(on Unix terminals the shell runs under `clud-remote-shell`, an argv[0] name
of `clud` itself; see [Request sudo](#request-sudo)).
The shell runs as the current user in the client's current directory. The
operator can read and modify anything that user can access. Only connect to an
operator you trust. Closing the session stops the shell; this does not undo its
commands or revoke credentials that an executed command copied elsewhere.

Client output is binary channel 0; server input is channel 1. JSON messages
carry capability negotiation, consent, terminal dimensions, exit status and
session state. Heartbeats run every 10 seconds. Control revocation clears local
approval and closes the PTY. Disconnect, denial, expiration and Ctrl+C terminate
the foreground client and close the PTY through its cleanup guard. An unexpected
loss of the connection does not: the session resumes (see
[Session resume](#session-resume)). Client output
and control events are not written to files by clud.

The shell's output also goes to the client's own terminal, so the person who
consented can watch what the operator does (`remote_connect/console.rs`). The
view is watch-only: the operator's terminal answers queries and reports input.
`remote_connect/mirror.rs` passes drawing through unchanged and removes, with a
streaming parser that survives split reads, every sequence that would make the
local terminal type into the client's stdin: queries (DA, DSR, XTVERSION,
DECRQM, XTWINOPS reports, colour/`?` OSCs, XTGETTCAP, DECRQSS), OSC 52 clipboard
access, the mouse/focus/bracketed-paste modes and keyboard protocols (kitty,
modifyOtherKeys). Forwarded, a mouse-tracking TUI made Konsole send a report per
pointer move into the client and, after it exited, into the user's shell.
Because the mirror is byte for byte, the PTY takes the local terminal's size
when the client has one and the server advertises `hello.pty_sizing:"client"`:
it ignores the operator's `pty_resize`, follows the user's window and announces
each size as `pty_size`, and the console renders that grid
(`remote_connect/size.rs`,
[DD-229](../DESIGN_DECISIONS.md#dd-229-the-consenting-users-terminal-sets-the-remote-sessions-size)).
Without a local terminal or against an older server the operator's size is
used. When the shell ends or control is revoked the client
undoes what the program left on (alternate screen, cursor, input modes) before
printing anything else, and turns every input-reporting mode off. A single worker owns stdout: past 2 MiB waiting
on a stalled local terminal, mirrored output is dropped and the gap is reported,
while the session and the server's copy carry on unaffected.

ConPTY holds a new shell until its first `ESC[6n` is answered. The Windows client
answers that one query and removes it from the stream: the operator's terminal may
not be attached or hold control, and a second reply would reach the shell as input.
ConPTY also keeps its output open after the shell exits, so the pump polls for exit
whenever output is idle rather than waiting for the reader's EOF.

Terminal writes run on a separate worker so a program that stops reading
input cannot prevent session cancellation. Operator input waiting for the shell
is bounded by bytes (1 MiB), not frames: mouse tracking sends a frame per pointer
move, and an 8-frame queue ended sessions mid-use. Past the budget a frame is
dropped and the operator gets one `pty_error` `input_dropped` per run of drops;
the session carries on. Output frames are at most 16 KiB (the server drops
larger ones), and output already waiting is coalesced into one frame, so a
progress bar's many small writes do not cost a frame each. The forwarding
queue holds at most 128 frames (2 MiB), and the session loop empties it on
every pass while the socket takes data. While the socket is backed up (see
[a slow link is waited out](#a-slow-link-is-waited-out)) the loop leaves the
queue alone; when it is full the pump waits holding the PTY reader's lock, so
running-process stops reading the PTY and the shell's own writes block: a
burst such as `uv` progress output is slowed to the socket's pace, never
dropped, reordered or allowed to end the session. Stopping the terminal
closes the PTY, which releases a waiting pump.

The server contract is maintained in `talos-hq/clud.bot`'s `SPEC.md`. This native
client advertises `pty`, not the legacy exec capability. The session creation
response supplies a short-lived client bearer and proxy model. The native client
validates that the bearer matches its same-origin WebSocket credential and derives
the inference URL from the user-selected origin, never from an arbitrary returned
URL. Its remote HTTP and WebSocket connectors combine bundled public roots with
system trust; unrelated HTTP clients keep their existing trust policy.

## `clud` in the operator's shell

`clud` typed into the operator's shell runs the binary serving the session.
When `remote_connect/terminal.rs` spawns the shell, it puts that binary's
directory first on the shell's PATH (`shell_environment`). The binary is
`std::env::current_exe()`. Off Windows, a symlink such as Homebrew's
`bin/clud` is followed to the real file, so the added directory holds clud
rather than every tool beside the link. The rest of PATH keeps its order. An
entry already first is left alone; one later in PATH moves to the front, so
it is never listed twice. The key is matched case-insensitively, so Windows'
`Path` is updated in place, and the separator is the host's (`;` or `:`).

The one-liners make this necessary. `connect.ps1` runs a verified copy from
`%LOCALAPPDATA%\clud-connect-<guid>\bin`, and `connect.sh` one from
`.clud-connect.XXXXXX/bin`; neither touches PATH. Without the prepend, `clud`
in the shell was "not recognized", or resolved to some other install. The
PTY's environment replaces the client's environment rather than overlaying
it, in running-process on every host, so the list built here is the shell's
whole environment.

An environment prepend is enough because the shell reads no startup files.
A session always has its proxy environment, so the shell is `cmd.exe /d`
(no AutoRun) or `/bin/sh` (no profile or `ENV`). A login shell's
`/etc/profile` would reset PATH, but none runs. Under
[Request sudo](#request-sudo), `clud-remote-shell` passes its environment to the
shell unchanged, so PATH is the same there. The leader's own `sudo` calls keep
their fixed `SUDO_PATH`, because the operator controls the user's PATH.
See [DD-221](../DESIGN_DECISIONS.md#dd-221-the-remote-shell-finds-clud-through-the-client-not-the-connect-scripts).

## Request sudo

The operator can ask for administrator access from the admin console's
**Request sudo** button. The server sends `sudo_request {request_id}`; the
client answers only with `sudo_state {request_id?, state, until?}`, where
`state` is one of `prompting`, `active`, `denied`, `unavailable`, `dropped`
or `expired` and `until` (Unix seconds) accompanies `active`. No free text
and never a password. **Drop sudo** sends `sudo_drop`. The client advertises
the `sudo` capability only on Unix and only when stdin and stdout are a
terminal (`sudo::advertised`), because the password must be typed hidden.
Windows clients never advertise it; the console disables the button with
"sudo is not available on Windows".

**Why a session leader.** Under sudo's default `timestamp_type=tty`, a
ticket is keyed by uid, tty, session ID and the session leader's start time.
The client is not in the operator PTY's session and cannot join it
(`TIOCSCTTY` from another session fails with `EPERM`), so it cannot validate
on the operator's behalf. When the client offers `sudo`, the PTY's first
process is therefore `clud-remote-shell`, an argv[0] name of the one `clud`
binary (`multicall.rs`; the client places the alias in a private 0700
directory and runs it from there). It connects back to a socket in that
directory, proves itself with a nonce from its environment, and waits. The
client accepts exactly one connection whose peer is the PTY's process and a
session leader, acknowledges it, and removes the socket and the alias. Only
then does the leader start the user's shell, as its child in the same
session. No operator-controlled process exists before the handshake ends.
On Linux the leader makes itself non-dumpable (`PR_SET_DUMPABLE`), so the
same-uid operator cannot ptrace it or read its memory or environment. On
macOS the signed release binary's hardened runtime refuses debuggers. Its
socket is close-on-exec, so the shell never inherits it.

**Flow.** On `sudo_request` the client sends `prompting` and asks the leader
to probe (`sudo -n -v`). If no password is needed it reports `active` without
`until`. If the user has no sudo rights, or there is no root-owned setuid
sudo at `/usr/bin/sudo`, `/bin/sudo` or NixOS's `/run/wrappers/bin/sudo`
(PATH is never used), it reports `unavailable`. Otherwise the user sees,
with mirrored shell output held back until the question is answered:

```text
Zach Vorhies is asking for administrator (sudo) access for up to 4 hours.
Changes made as root stay after sudo ends.
Allow sudo for Zach Vorhies? [y/N]:
```

Anything but yes reports `denied` without asking for the password. On yes:

```text
Password for <user> (typing is hidden; it is never sent to clud.bot):
```

Echo is off for that line (`platform::console_echo`: `ECHO` cleared,
`ECHONL` set, `ICANON` and `ISIG` kept, applied with `TCSANOW`). The guard
restores it when the line arrives, on revoke, disconnect, Ctrl+C or a panic.
The line moves into a zeroized buffer, goes over the private socket, and is
zeroed. A copy can remain in the standard-input reader's internal buffer
until it is overwritten. The leader runs `sudo -S -p '' -v` with the
password on a pipe, then EOF, a clean environment (`PATH`, `LC_ALL=C`) and
pipes for every stream, so nothing sudo prints reaches the PTY. A wrong
password asks again, up to three attempts (`passwd_tries`), then reports
`denied`. A grandchild probe confirms that the ticket is shared with the
session; under `timestamp_type=ppid` or a zero timeout it is not, and the
leader runs `sudo -k` and reports `unavailable`.

**Lifetime.** The leader refreshes with `sudo -n -v` every 60 seconds and
stops at 4 hours after approval: it runs `sudo -k` and reports `expired`, and
the operator must ask again. **Drop sudo** runs `sudo -k` and reports
`dropped`. Control revocation, Restart shell, disconnect and Ctrl+C drop sudo
before the PTY is killed. Losing the client hangs up the shell and drops
sudo, and the leader's exit invalidates the ticket on its own (it is bound to
the leader's start time). Only `sudo -k` is used, never `-K`, which would
also log the user out of sudo in their own terminals. The ticket does not
carry into `tmux`, `screen` or new terminals. Changes made as root stay.

The leader runs sudo with SIGTTOU and SIGTTIN ignored. The shell owns the
terminal's foreground, and sudo-rs touches the controlling terminal even
with `-S`; from the leader's background group the kernel stopped it. The
shell, started earlier, keeps the default dispositions.

**macOS** uses the same leader. The password is always read by clud and
passed with `-S`; Touch ID (`pam_tid`) is not offered. **Windows** is not
supported: UAC elevates one process at a time and a ConPTY shell cannot
become elevated. A UAC-elevated admin tab is tracked as a separate project.

The shell is started through running-process's caller-owned foreground API
(`running_process::foreground::spawn`) with a configuration-only `Command`
(the `command-builder` exception in `ci/banned_imports.py`), because
`NativeProcess` always captures stdout and the shell must inherit the PTY.
It is reaped with `waitpid`; sudo itself runs through `NativeProcess`.
[DD-216](../DESIGN_DECISIONS.md#dd-216-remote-sudo-is-validated-by-a-clud-session-leader-inside-the-operator-pty)
records why.

## Session resume

A clud.bot deploy restarts the server with downtime, and networks drop. Neither
ends a live session any more ([DD-218](../DESIGN_DECISIONS.md#dd-218-remote-sessions-resume-after-a-lost-connection-with-the-same-token-and-local-consent-stays-authoritative);
protocol: clud.bot `SPEC.md` §4.2.3; code: `remote_connect/resume.rs` and
`Link` in `remote_connect/mod.rs`).

**Loss.** The session loop (`Link::online`) ends a connected phase on a socket
error, a close frame (except 4409 and protocol violations), a `server_restart`
notice, or a heartbeat timeout: no frame from the server for 35 seconds while
pinging every 10, which catches a half-open connection with no FIN. A slow or
stalled link alone is not a loss ([below](#a-slow-link-is-waited-out)). Expected
terminations still end at once with their own messages: `disconnect`,
`denied`, `expired`, `ended`, and a refused reconnect whose
`/api/remote/status` is terminal. A server that did not advertise `resume` in
its `hello` gets the old behaviour: the client exits with
`remote connection lost`.

**Reconnect.** The client shows `Connection to clud.bot lost; reconnecting…
(Ctrl+C to end)` once and keeps the shell running. On a terminal it is a
banner: drawn over the bottom row with the cursor saved and restored, cut to
the terminal's width and never followed by a newline, so the remote program's
screen does not scroll and its wrapping mode is untouched. Anything written
while it shows (mirrored output, a notice, a question) erases it first and
redraws it after, so it cannot scroll up into the program's screen; the resume
erases it for good. The remote program keeps running and drawing through the
outage, so its terminal modes are left alone (no alternate-screen or cursor
reset). After the banner is erased the PTY is made one row shorter for 200 ms,
which makes the program (or ConPTY) draw its whole screen again; the size is
then restored (and announced again when this terminal sets it), unless a
resize arrived meanwhile. Piped output gets plain `…reconnecting…` and
`Reconnected.` lines instead. `Reconnector` attempts in a
background thread so the loop keeps draining the PTY and sees Ctrl+C even
while a handshake waits on an unreachable host. Backoff is capped exponential
with jitter: 0.5 s doubling to 15 s, each wait 50-100 % of its step; the first
attempt after a `server_restart` waits about 2 s. It gives up after
`RESUME_WINDOW` (5 minutes; `CLUD_REMOTE_RESUME_WINDOW_SECS` may only shorten
it, for tests) with a non-zero exit. Every connection, the first included, uses
`ws_url&resume=1` with the same client token; the server lets the newest such
connection replace an older one and closes the older with 4409, which the
client treats as final ("This session was resumed from another connection.").
A deliberate exit closes with 1000 so the server ends the session at once
rather than waiting out its grace window.

The client remembers the last usable TLS WebSocket address in memory for the
current session. On reconnect, it gives DNS 500 ms to return fresh addresses
before falling back to the remembered address. A slow lookup remains pending
across retries, so a later reconnect can still use its answer. The dial
interleaves IPv4 and IPv6 candidates and starts them 250 ms apart. TCP and
TLS can run in parallel, while each WebSocket upgrade uses the session token
in sequence. Every candidate keeps the original hostname for TLS and the
upgrade, and the chosen socket still completes the required end-to-end
handshake before use.

**While disconnected** the PTY's output is still drained at the local
terminal's pace, mirrored there and held in `Outage`, at most 1 MiB, oldest
dropped first; the `pty_exit` of a shell that ended offline is never dropped.
Unlike the connected path, which slows the shell to the socket's pace (see
above), nothing paces an outage of unknown length, so the operator gets the
newest output and a `pty_gap` marker for the rest. A consent answer typed
while offline is remembered, not sent.

**Resync.** On the new `hello`, `State::resume` reconciles with the server's
record (`hello.resume`). Until it has, nothing of the shell's (held or new) is
sent on the new socket, and the shell waits. Local consent stays authoritative:

- control continues only if this client holds the grant whose request id the
  server records (`grant_id`); otherwise the client closes its terminal,
  prints "Remote control ended while the connection was down.", and sends
  `control_release` if the server still shows a grant;
- a grant the server made for the user's own yes, whose `control_granted` was
  lost with the socket, is taken up; nothing else grants control;
- a pending question with the same request id is not asked again; an answer
  typed while offline is sent if that request is still open; a request the
  operator withdrew clears the question;
- sudo keeps running in the session leader while the client is away; the
  client re-reports where it stands (`Sudo::report`: `prompting` or `active`
  with `until`), or the last `sudo_state` that could not be sent, or
  `dropped` when the server still shows sudo on and nothing is running here.
  A server restart forgets sudo state, so this is what restores the console's
  view; revoking control while away drops sudo with the shell;
- the client reports the terminal it still holds (`resume`), then sends the
  held output, preceded by `pty_gap` when output was dropped, and prints
  `Reconnected.` If that socket fails during replay, any gap or frames the
  socket rejected stay queued with their PTY identity for the next resume.

**Proxy mode** (`clud connect` → Use Proxy) reconnects the same way in the
background; the user's agent keeps running and is not interrupted.

### A slow link is waited out

On a poor network the link is often slow or briefly stalled rather than gone,
and reconnecting then costs more than it saves: the new handshake crosses the
same bad link, and whatever the old socket had in flight is lost, because the
protocol has no acknowledgements to replay it from
([DD-232](../DESIGN_DECISIONS.md#dd-232-a-remote-session-waits-out-a-slow-link-on-a-non-blocking-socket)).
So after the handshake the session socket (`transport::Socket`) is
non-blocking. A message the network cannot take yet stays queued in order in
tungstenite and goes out on a later pass (`Socket::pump`); while anything is
queued (`backed_up`), the loop reads no more shell output, model-bridge frames
or held outage frames, so the queue stays short and a heartbeat ping is not
stuck behind megabytes of output. Only the heartbeat decides the link is gone.
TLS runs through `tls::TlsStream` rather than rustls's own stream, which suits
only blocking sockets: its send buffer is capped at 64 KiB and a write past
the cap returns `Ok(0)`, which tungstenite reports as a reset, so a model
request larger than the cap ended the connection whenever the network was full;
and its reads first finish pending writes, so a full uplink also held back the
operator's keystrokes and the server's pongs. `TlsStream` accepts every write
(the pacing above keeps its buffer small) and reads whatever has arrived while
writes wait.
An idle pass sleeps 1 ms, doubling to 20 ms (`transport::Idle`), so a
keystroke is still answered within 20 ms. `TCP_NODELAY` is set, so a
keystroke's echo is sent without waiting for an acknowledgement. Held outage
output is sent at the link's pace after a resume, ahead of newer output. A
deliberate exit waits up to 2 s for its close frame to get through.

A blocking socket with timeouts, the earlier design, abandoned a link whose
writes stalled for 5 s; on a lossy link TCP's own retransmission backoff passes
that easily. It was also unsafe on Windows, where a send or receive that times
out leaves the socket in an undefined state.

**Startup.** Creating the session and opening its first socket retry a
network failure (and a 408, 429 or 5xx answer) with the same backoff for up to
60 s, saying so once on stderr; `CLUD_REMOTE_STARTUP_WINDOW_SECS` may only
shorten it. A refusal, a bad response and a server that will not encrypt still
fail at once. A TLS failure, a rejected certificate included, counts as a
network failure and is retried, as it is on reconnect: a captive portal or an
intercepting middlebox on a poor network looks like one, nothing is sent before
the certificate is verified, and the retry ends when the network does.
No keyword is shown before creation succeeds, so a retry creates nothing the
user has seen.

## End-to-end encryption

clud.bot's hosting edge terminates TLS, so TLS alone lets it read every frame
on the session socket. `remote_connect/e2e.rs` adds a second layer inside it
([DD-219](../DESIGN_DECISIONS.md#dd-219-the-cludbot-session-socket-is-end-to-end-encrypted-with-an-unauthenticated-ephemeral-exchange-inside-tls));
the wire format is owned by clud.bot's `SPEC.md` §4.2.4.

- **Exchange.** The client adds `e2e=1` to the socket URL
  (`transport::socket_url`). A server that offers encryption greets with a
  plaintext `e2e_offer` (its ephemeral X25519 key and a nonce) instead of
  `hello`; the client answers `e2e_accept` with its own, and both derive one
  ChaCha20-Poly1305 key per direction with HKDF-SHA256, salted with the
  transcript hash (`e2e::transcript`). It runs inside `transport::connect_url`
  before the socket is handed out, so every first connect and every resume
  (`resume::Reconnector`) gets fresh keys.
- **Records.** `transport::Socket` wraps the WebSocket: `send` seals each text
  or binary message, `read` opens it, so `mod.rs`, `connect.rs` and
  `bridge.rs` see plaintext and are unchanged. A record that fails to open
  (altered, replayed, reordered, truncated, or a plaintext frame) makes
  `read` close with 4400 and return that close; `resume::close_meaning`
  treats 4400 as final, so the session ends with
  `an encrypted frame from clud.bot failed authentication` instead of
  reconnecting. The server does the same for frames from the client.
- **Downgrade.** `Session::e2e_required` is set before the first connect for
  `clud.bot` (`ENCRYPTED_HOSTS`), when the creation response carries `e2e`,
  or with `CLUD_REMOTE_REQUIRE_E2E=1`, and after any encrypted socket. A
  server that greets in plaintext is then refused
  (`ConnectError::Insecure`, final on reconnect too). Otherwise the client
  prints a one-line stderr warning and continues in plaintext, as old servers
  require.
- **What stays visible to the edge.** Session timing; the creation request
  (`clud_version`, `os`, and, unless the client already expects encryption,
  computer name, home-folder user name and directory, which otherwise go
  inside the tunnel as `client_info`; the operator console labels the
  session with the first two, #2899); the creation response (keyword, bearer, `ws_url`, model);
  the bearer in the socket URL; the exchange's public values; every record's
  size, direction and timing; close codes; and the operator console leg below.
- **Not defended:** an edge that actively swaps both ephemeral keys. The
  exchange is unauthenticated by design (no server identity key); see the DD.
- **The operator leg.** The admin console's WebSocket (browser ↔ server)
  also crosses the edge in TLS only, so what the operator sees (PTY output,
  chat) is still readable there. Extending encryption to it means the
  operator's browser holds its own ephemeral keys and runs the same exchange
  with this client, with the server relaying sealed PTY and chat frames it
  cannot open. The client ↔ server channel stays as well, because model-proxy
  traffic must terminate at the server, which calls OpenRouter.

Tests: `e2e_tests.rs` (known-answer vector shared with clud.bot's
`tests/test_e2e.py`, tamper at every byte, replay, reorder, truncation,
transcript binding, strict offer parsing, low-order keys); `transport.rs`
(policy); clud.bot's `tests/e2e/native_e2e.py` drives this binary through a
WebSocket-aware tap that captures, tampers, strips the request and cuts the
link.

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

The endpoint outlives a socket. When the session resumes after a lost
connection, requests that were in flight are answered `502` at once (their
server half went with the old socket; the agent retries them) and their
queued frames are dropped. New requests wait for the resumed socket.

Proxy disablement leaves terminal control intact but invalidates inference on
the server. Disconnect, denial and session end revoke the bearer and close the
shell. The configuration-directory guard lives until the foreground remote
session ends. A key request still receives an explicit unsuccessful response:
this command never persists an OpenAI key.

## Validation

`cli::remote_connect_e2e_cli` runs the actual client and a real shell against an
in-process operator server on every host, without clud.bot or credentials.
Its private-copy case copies clud to where the one-liners put it, starts it
with only system directories on PATH, and checks that `clud --version` typed
by the operator prints this build's version.
Its terminal-hosted case runs the client in a PTY, so it offers `sudo` and
starts the shell under `clud-remote-shell`. The leader's own tests
(`platform::unix::remote_shell::tests`, Linux) drive it in a real PTY session
with a stub sudo that keys tickets by session ID: wrong password, success,
sharing with the operator's shell and a nested script, drop, keep-alive,
expiry at the cap, the shell's exit code, losing the client, and a
non-dumpable leader. With `CLUD_TEST_REAL_SUDO_PASSWORD` set, in a throwaway
container whose user may use sudo, the same tests and the terminal-hosted
end-to-end case use the system's setuid sudo.
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

# remote_connect/

Native client for `clud clud.bot` (the fixed-host alias), `clud remote connect clud.bot` and `clud connect clud.bot`
(`connect.rs`: the mode prompt and the proxy-only session). See
[remote assistance](../../../../docs/architecture/remote-assistance.md) for the
consent boundary, protocol and executable integration check.

- `mod.rs`: CLI action, foreground session (`Link`: connected and
  reconnecting phases), heartbeat and local consent state, including the
  consent reconciliation after a resume.
- `resume.rs`: reconnecting after an unexpected loss: the resume window,
  jittered backoff, close-code meaning, the bounded outage buffer and the
  background `Reconnector`. See
  [session resume](../../../../docs/architecture/remote-assistance.md#session-resume).
- `console.rs`: the stdout worker: mirrors shell output to the client, prints
  notices and restores the local terminal, never blocking the session loop.
- `mirror.rs`: what of the shell's output the local terminal may see: drops
  queries, clipboard writes and input-reporting modes, keeps all drawing.
- `transport.rs`: bounded session creation, same-origin bearer validation and
  HTTP/WebSocket TLS with bundled plus system roots.
- `e2e.rs`: end-to-end encryption of the session socket inside TLS
  (ephemeral X25519, HKDF-SHA256, ChaCha20-Poly1305 records); `transport::Socket`
  seals and opens every frame. See
  [end-to-end encryption](../../../../docs/architecture/remote-assistance.md#end-to-end-encryption).
- `bridge.rs`: loopback model endpoint that carries the agent's requests over
  the session socket (channel 2), past the hosting edge's HTTPS body firewall.
  See [model requests ride the session socket](../../../../docs/architecture/remote-assistance.md#model-requests-ride-the-session-socket).
- `proxy.rs`: ephemeral inference environment, credential isolation and
  foreground-only nested launch selection before local credential access.
- `terminal.rs`: consented shell PTY, binary streams, resize and cleanup through
  running-process and the existing platform boundary. The shell gets the
  running clud's directory first on PATH
  ([contract](../../../../docs/architecture/remote-assistance.md#clud-in-the-operators-shell)).
- `size.rs`: whose terminal sets the PTY's size: the local terminal's, when
  there is one and the server follows (`pty_size`), else the operator's. See
  [DD-229](../../../../docs/DESIGN_DECISIONS.md#dd-229-the-consenting-users-terminal-sets-the-remote-sessions-size).
- `sudo.rs`: "Request sudo" on the client: the `[y/N]` consent, the hidden
  password prompt, and the closed `sudo_state` set the server may hear.
- `leader.rs`: the portable half of `clud-remote-shell`, the PTY's session
  leader (`platform::remote_shell`): the client <-> leader line protocol,
  sudo output classification, the 60 s keep-alive and the 4-hour cap. See
  [Request sudo](../../../../docs/architecture/remote-assistance.md#request-sudo).
- `open_url.rs`: "Open Address": opens an operator-sent URL in the user's
  browser only while remote control is granted, and only `https://` or a
  loopback `http://` origin; the console reports every open or refusal.
- `paste.rs`: drops bracketed-paste markers from operator input while the
  remote shell has not enabled the mode, so a console paste never reaches the
  shell as `^[[200~...~`.

Called by `main.rs` before backend selection; registered in `args.rs`.

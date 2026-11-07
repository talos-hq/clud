# remote_connect/

Native client for `clud remote connect clud.bot` and `clud connect clud.bot`
(`connect.rs`: the mode prompt and the proxy-only session). See
[remote assistance](../../../../docs/architecture/remote-assistance.md) for the
consent boundary, protocol and executable integration check.

- `mod.rs`: CLI action, foreground session, heartbeat and local consent state.
- `console.rs`: the stdout worker: mirrors shell output to the client, prints
  notices and restores the local terminal, never blocking the session loop.
- `transport.rs`: bounded session creation, same-origin bearer validation and
  HTTP/WebSocket TLS with bundled plus system roots.
- `proxy.rs`: ephemeral inference environment, credential isolation and
  foreground-only nested launch selection before local credential access.
- `terminal.rs`: consented shell PTY, binary streams, resize and cleanup through
  running-process and the existing platform boundary.

Called by `main.rs` before backend selection; registered in `args.rs`.

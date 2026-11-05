# remote_connect/

Native client for `clud remote connect clud.bot`. See
[remote assistance](../../../../docs/architecture/remote-assistance.md) for the
consent boundary, protocol and executable integration check.

- `mod.rs`: CLI action, foreground session, heartbeat and local consent state.
- `transport.rs`: bounded session creation, origin validation and TLS WebSocket.
- `terminal.rs`: consented shell PTY, binary streams, resize and cleanup through
  running-process and the existing platform boundary.

Called by `main.rs` before backend selection; registered in `args.rs`.

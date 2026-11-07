# Gateway test modules

The production gateway remains in `../codex_bridge.rs`.
`tests/redaction_http.rs` is a child of its test module and reuses the local
fake HTTP upstream, request builder and transport assertions. It verifies
redaction on actual gateway requests without external providers or credentials.

`tests/route_history_http.rs` covers side-call isolation before unified routing,
opaque Codex reasoning across proxy failover/recovery, intervening proxy replay,
Codex quota failures, and compaction while proxy replay is pending. Genuine user
provider switches remain covered by the
provider-switch tests in the parent module.

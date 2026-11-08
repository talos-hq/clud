# Secret redaction

`detect.rs` owns precise credential ranges and overlap precedence.
`policy.rs` resolves flags, free-model defaults and known training policy.
`session.rs` owns the HMAC key, zeroizing mapping, JSON walk and counters;
`reversible.rs` exports that API and carries its core regression tests.
`stream.rs` restores Anthropic SSE with bounded trailing prefixes per block.
`status.rs` publishes counts when a gateway turn finishes, including failures.

The mapping has no Debug or serialization surface. Only `Counts` may enter
status state. HTTP boundary tests live in
`../codex_bridge/tests/redaction_http.rs`; final Codex send/retry/compaction
coverage lives in `../codex_upstream.rs`.
See [the architecture contract](../../../../docs/architecture/secret-redaction.md).

`logging.rs::redact_for_log` is the pure, one-way persistence API. It reuses
`detect.rs` and adds log-only argument, header, quoted Apple password and
`gh secret set` literal contexts. It emits `[REDACTED:<kind>]` without a
session map. The hook log and selected structured records use this API. Field selection
and the remaining #2210 scope are documented in the architecture contract
above. Operational inputs never pass through it.

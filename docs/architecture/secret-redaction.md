# Reversible gateway secret redaction

Redaction belongs to the foreground unified gateway. `--redact-secrets`
enables it; `--no-redact-secrets` disables it. Without an override it is on
for OpenRouter wire model IDs ending in `:free` and explicitly training-allowed
routes. A low price alone does not establish a provider's training policy.
`--dry-run` reports `redaction.enabled`, `reason`, and `patterns`.

A Claude harness launch that needs redaction selects the unified gateway
before backend arguments are rendered. A native Codex launch cannot use that
request path and fails its launch check with a diagnostic naming `--unified`.
A launch cannot silently accept a redaction flag while sending directly.

## Secret ownership

`secret_redaction::Session` owns a random 256-bit HMAC key, resolved vault
values, and a token-to-secret map. Its owned key and credential buffers use
`Zeroizing`; the session has no Debug or serialization implementation.
The mapping stays in the foreground process and is discarded with it. It is
never included in launch plans, daemon messages, status files, or logs.

Each detected value becomes `clud_secret_<12 hex>` using HMAC-SHA256 under
the session key. Repeated values keep the same token; collision resolution
checks the existing mapping before choosing a new token. Different launches
have independent keys. This preserves prompt prefixes within a launch without
publishing the original value.

Detection uses named credential prefixes, PEM private-key blocks, and exact
resolved vault values. It does not infer secrets from entropy, personal names,
email addresses, IP addresses, checksums, UUIDs, or ordinary code identifiers.
System and message text, nested tool results and historical tool inputs, and
text attachments are scanned, including object member names in tool JSON.
A transformation that would merge object keys fails closed. Image payloads and binary document sources
are left alone. Clean JSON requests retain their original bytes.

## Request and response boundaries

Anthropic-compatible requests are redacted before their HTTP send. The Codex
upstream client also redacts at its final send boundary, after translation,
so canonical history, retry bodies and compaction cannot bypass detection.
Each attempted failover rung evaluates policy before sending. A turn retains
its redaction session once any rung requires it, including a paid-to-free
transition; explicit disable remains authoritative.
Provider authorization headers retain the actual credential required to
authenticate; redaction applies to model-visible request content.

Nonstreaming response JSON is restored before delivery to the harness.
Anthropic SSE text and tool-argument fragments keep at most a trailing token
prefix per content block. Block/message stop flushes incomplete prefixes.
Tool-argument restoration escapes quotes, backslashes and control characters
for JSON. Unknown tokens pass through unchanged and increase a counter;
no diagnostic prints their corresponding values.

Completed gateway turns publish aggregate counts per detector kind and an
unknown-placeholder count through the existing status writer. The status
line shows totals; the serialized counters contain neither token mappings
nor credential values. Concurrent completion reports cannot decrease counts.

## OpenRouter guardrails

When clud redaction is enabled, configure OpenRouter's own secrets guardrail
to **Block**, or turn it off. Avoid provider-side **Redact**: it is one-way,
can replace ordinary code, and cannot restore values in generated file edits.
Local reversible redaction and provider routing/privacy controls are separate
features; redaction does not prove zero data retention or disable training.

## Validation

Core detector/JSON/token tests live in `secret_redaction/reversible.rs`;
SSE split, interleaved-block and reader tests in `secret_redaction/stream.rs`;
status secrecy in `secret_redaction/status.rs` and `toast/statusline_tests.rs`;
CLI route-policy assertions in `tests/test_secret_redaction.py`.
Run them through the repository's Bosn CI path. Successful core tests alone
do not establish retry, failover, or gateway HTTP integration coverage.

## One-way structured-record persistence (#2210)

`secret_redaction::redact_for_log` also owns shape-based persistence redaction.
It emits `[REDACTED:<kind>]` without a session mapping. Writers select only
text fields; the field selector adds no credential patterns or policy.
Operational commands, arguments and decisions never use the rewritten copy.

| Writer | Redacted fields | Preserved fields |
| --- | --- | --- |
| Shim telemetry | `argv` | Tool, cwd, session identity, parent, pid, times and exit code |
| Launch record | `command`, `clud_argv`, `failure_reason`, `metered_warning` | Launch ID, source, backend, mode, cwd, repo root, pid, times and exit code |
| Bridge JSONL | `detail` | Correlation/model IDs, classifications, retry decisions, counts and times |
| Daemon events | `error`, `write_error`, `wait_error`, adopt-kill `reason` | Event/request/session IDs, paths, pids, labels, fixed reasons, counters and times |
| Session tool index | Started-event `args` | Tool identity, lifecycle reasons, tool ID, pids, exit code, schema and times |
| GC required/best-effort audit | `rule` (may include a human root-override explanation) | Site, exact filesystem path, event and timestamp |
| rm audit | Handoff argv, top-level `reason`, per-outcome `reason` | Operands (filesystem paths), command identity, paths/trash paths/roots/cwd, role, session, flags, actions, exit code and time |

GC selection rules can include the human reason from `safe_rm.extra_roots`,
so both required and best-effort writers redact that text. Settings audit
contains key pointers, protected roots,
target, outcome, pid and time, with no setting values. Reap JSONL contains
process identity (including image name), action, reason and phase; its flight
recorder contains counters. These records remain unchanged.

Remaining work includes test-capture artifacts, output tee/caps and raw CI-log
continuations (byte-exact/verdict contracts); session logs/transcripts, crash
reports, workflow replay cache and Python tool artifacts; known-value matching,
historical scrubbing, the hook guard and high-entropy detection. This slice
neither rewrites historical state nor broadens the detector from #2711.

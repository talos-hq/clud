# Session origin and idle retention

Every durable API session (`ApiSessionRecord`, see
[api-session-storage.md](api-session-storage.md)) carries one typed field,
`origin: SessionOrigin` (`crates/clud-bin/src/session_origin.rs`):
`cli | mcp | daemon | test | unknown`.

## Set once, at creation

The entry point that creates the session sets `CreateApiSession::origin`;
nothing infers it later from argv or environment. Over HTTP the creating
client declares it in the `POST /v1/sessions` body (`"origin": "mcp"`);
the MCP server's `session_start` always sends `mcp`. A body without the
field, and every record written before the field existed, reads as
`unknown` (`#[serde(default)]`). No migration rewrites old records, and
there is no side table, tag list or environment variable.

## Surface

- `clud list` shows an `ORIGIN` column in the API-session table.
- `clud list --json` prints API sessions as a JSON array; every row has
  `origin`.
- `clud list --origin <cli|mcp|daemon|test|unknown>` shows only API sessions
  of that origin. Any other value is a clap parse error listing the valid
  values.
- `clud session show <id> [--json]` prints origin with state, backend and
  model.
- The HTTP `Session` object includes `origin`.

## Idle sessions are never reaped

`ApiSessionState::is_reapable` is true only for `terminated` and `failed`.
`ApiSessionStore::gc` deletes only those records; the daemon runs it at
startup, before restart reconciliation, so a turn interrupted by that very
restart stays resumable. An `idle` session holds a resumable conversation and
is kept deliberately: the instance cap, not GC, is the pressure valve.
Guarded by `daemon::api_session_origin_tests`
(`idle_cli_session_survives_gc_and_terminated_one_is_reaped`,
`gc_reaps_only_terminated_and_failed_states`).

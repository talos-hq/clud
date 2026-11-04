# Server-side settings

Some values in clud must be changeable without a release, so they are served
from this repository (#1192,
[DD-072](../DESIGN_DECISIONS.md#dd-072-server-side-settings-are-one-baked-in-json-document-with-per-section-last-known-good)).
Today the served values are DeepSeek's model names (#1192), the OpenRouter
model-context map (#1258), and the `per_turn_effort` enable flip (#1528). The
larger OpenRouter pricing catalog (#1256) is a separate document with its own
cache and consumer.

## The document

`crates/clud-bin/assets/server-settings.json` is the only source. The build
embeds it as the **built-in copy**, and installed builds fetch the same path
from `main`:

```text
https://raw.githubusercontent.com/zackees/clud/main/crates/clud-bin/assets/server-settings.json
```

```json
{
  "schema_version": 1,
  "sections": {
    "deepseek": { "default_model": "deepseek-flash", "subagent_model": "deepseek-flash[1m]" },
    "model_contexts": { "xiaomi/mimo-v2.6-flash": 1048576, "~anthropic/claude-sonnet-latest": 1000000 },
    "per_turn_effort": { "enabled": false }
  }
}
```

- **`schema_version`** is the document format. A build accepts documents up to
  its own `SCHEMA_VERSION`. Additive changes never bump it. A bump makes every
  older build ignore the whole document and keep its last good copy, so treat
  it as a last resort and prefer adding a new section key.
- **`sections`** holds one object per setting. Each section is validated on its
  own: first a typed decode that ignores unknown fields, then the section's
  `validate()`.
- **Any other top-level key** is ignored.

## Resolution

Each registered section takes the first valid value from this list:

| Order | Source | Notes |
| --- | --- | --- |
| 1 | The copy served now | Used only when a refresh finished within the launch's wait |
| 2 | The last cached valid copy | `~/.clud/cache/server-settings/server-settings.json` |
| 3 | The built-in copy | Always valid; the guard tests enforce this |

A process resolves its settings once, through `server_settings::snapshot()`. Every
consumer in that process then sees the same values, even if a refresh lands
later. Loading is lazy: a launch that never reads a setting never touches the
cache or the network.

### Merging a served document

| Served section | Result in the cache |
| --- | --- |
| Valid | Replaces the cached value |
| Present but invalid (wrong type, or `validate()` fails) | Keeps the previous valid value; with no previous value, the section is dropped and the built-in value applies |
| Absent or `null` | Dropped, so the built-in value applies. This is how you reset an override |
| Unknown to this build | Kept verbatim, so a newer clud sharing the cache still receives it |

A **whole-document** failure discards the served document and leaves the cache
untouched. That happens when:

- the body is not strict JSON;
- the root is not an object;
- `schema_version` is missing or unsupported;
- `sections` is missing or not an object.

The cached document is re-validated on every read. A corrupt cache is ignored
rather than trusted, and the next good refresh replaces it.

## Refresh timing

This lives in `server_settings/store.rs`.

- **Fresh cache.** A cache younger than 15 minutes is used with no request.
  `raw.githubusercontent.com` itself caches for five minutes, so polling more
  often would not surface edits any sooner.
- **Stale cache.** A named thread (`clud-server-settings-refresh`) fetches with a
  5 s timeout, merges, and writes the cache by staging `.json.partial` and
  renaming it. The launch waits at most 750 ms for that thread:
  - If the refresh finishes in time, this launch uses the result.
  - If not, this launch continues on its cache, and the refresh still lands in
    the cache for the next launch.
- **Backoff.** Each attempt touches `server-settings.json.last-attempt`. After a
  failed attempt, clud does not retry for 15 minutes. A network that cannot
  reach GitHub therefore costs at most 750 ms per 15 minutes.

## Strict JSON

`server_settings/json.rs` parses both the served body and the cache. It
enforces these rules:

- **Encoding and size.** The input must be UTF-8 and at most 64 KiB. A leading
  byte-order mark is stripped.
- **One value only.** The input must be exactly one JSON value. Trailing
  content, comments, trailing commas, single quotes, and `NaN`/`Infinity` are
  rejected.
- **No duplicate keys.** Duplicate object keys are rejected at any depth.
  Without this check, `serde_json` would silently keep the last one.
- **Bounded nesting.** Depth is capped by `serde_json`'s recursion limit, so a
  nesting bomb returns an error instead of overflowing the stack.

A non-2xx response, a transport error, or a timeout counts as a fetch failure.

## Controls

| Variable | Effect |
| --- | --- |
| `CLUD_SERVER_SETTINGS=0` (also `false`, `off`, `no`) | Use the built-in copy only: no cache, no network |
| `CLUD_SERVER_SETTINGS_URL=<url>` | Fetch a draft or a mirror instead. The cache is bypassed, so the real file's last good copy is never overwritten |
| `CLUD_VERBOSE_SERVER_SETTINGS=1` | Print refresh and fallback diagnostics to stderr, including why a section was rejected |

## Changing a served value

1. **Edit `crates/clud-bin/assets/server-settings.json` in a PR.** The guard
   tests fail if the document is not strict JSON, if a registered section is
   missing or invalid, or if a section is not registered.
2. **Merge to `main`.** Installed builds see the change within about 20
   minutes: up to 5 minutes of CDN caching plus up to 15 minutes of local cache
   freshness. It applies from the next launch after that.
3. **Optionally, try it first.** Serve the draft file and point
   `CLUD_SERVER_SETTINGS_URL` at it.

The same edit also becomes the built-in copy in the next release.

## Adding a setting

1. **Define the section type.** Write a `Deserialize` struct and implement
   `server_settings::Section` for it, providing `KEY` and `validate()`. Keep
   the values data-only: never serve credentials, code, or URLs that clud would
   fetch.
2. **Register it.** Add `SectionSpec::of::<YourSection>()` to `SECTIONS` in
   `server_settings/sections.rs`.
3. **Give it a built-in value.** Add the value under `sections` in the embedded
   JSON.
4. **Read it.** Call `server_settings::snapshot().section::<YourSection>()`, or add
   a cached accessor like `server_settings::deepseek()`.

## The `model_contexts` map (#1258)

`model_contexts` is a flat `{ "<wire-id>": <context window in tokens> }` map of
OpenRouter context windows as served by the default-routed endpoint. Claude Code clamps auto-compact to 200k for
any model its own catalog does not describe, so without this map every newly
listed OpenRouter model — starting with `xiaomi/mimo-v2.6-flash`, a 1M model —
compacts far too early.

- **Who reads it.** `server_settings::effective_context_window(wire_id)`,
  called by the Anthropic-compat overlay in `foreground_runtime.rs`, which sets
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS` for the launched wire ID. Resolution order:
  a catalog row's reviewed `claude_max_context_tokens` wins when it has one;
  otherwise the served map row; an ID in neither source emits nothing. An
  ambient user-set `CLAUDE_CODE_MAX_CONTEXT_TOKENS` is preserved, and
  `CLAUDE_CODE_AUTO_COMPACT_WINDOW` stays catalog-owned (DeepSeek, Kimi).
- **Who writes it.** `ci/refresh_model_contexts.py`, run daily at 04:17 UTC by
  `.github/workflows/refresh-model-contexts.yml` — this repository's first
  scheduled workflow. It fetches `https://openrouter.ai/api/v1/models` with
  stdlib `urllib`, rewrites **only** this section, and exits non-zero when the
  datasheet cannot be fetched or parsed, so a run fails loudly instead of
  publishing an empty or stale map. Before committing, the job runs
  `tests/test_refresh_model_contexts.py` against the regenerated file and
  stops on a failure. The refreshed file reaches `main` through a bot PR with
  auto-merge — a direct push is rejected because `main` requires the merge
  queue (#2518) — so installed builds see it through the normal
  fetch-from-`main` path once `CI OK` passes. The Rust `server_settings::` tests are not run by the bot
  ([DD-127](../DESIGN_DECISIONS.md#dd-127-scheduled-refresh-bots-gate-their-commit-on-the-producer-tests-not-on-a-pr)).
- **Which window.** The served window, `min(context_length,
  top_provider.context_length)`, not the maximum over endpoints, so the map
  never overstates what the default-routed endpoint accepts (#1634,
  [DD-126](../DESIGN_DECISIONS.md#dd-126-model_contexts-publishes-the-default-routed-endpoints-window-not-the-endpoint-maximum)).
  Rows without a top provider keep `context_length`.
- **Bounds.** Keys are wire IDs: 1..=128 bytes of `[A-Za-z0-9._-/:~]`. Values
  are `1_000..=10_000_000` tokens. The producer and `ModelContexts::validate`
  mirror each other; changing one without the other turns CI red.

## OpenRouter pricing catalog (#1256)

`assets/openrouter-catalog.json` is a separately embedded fallback and the
scheduled job's output. `openrouter_catalog::catalog` fetches only the fixed
raw GitHub document, disables redirects, and uses a three-second timeout. A
validated response atomically updates the catalog cache under daemon state.
Network or schema errors keep the last valid cache, then use the embedded
catalog. Cache entries refresh after six hours. The consumer never follows
URLs from the remote document.

The producer runs daily at 04:37 UTC and fails the workflow on fetch,
normalization, or serialization errors. It then runs `tests/test_refresh_openrouter_catalog.py`
before committing (DD-127). It publishes schema version 1 with
sorted raw model rows, OpenRouter source attribution, and a convenience
shortlist. Additive fields do not change the version; Rust ignores unknown
fields so older builds can read a newer additive document.

Eligibility is a capability and price filter, not a code-quality claim: a row
needs text input and output, `tools` and `tool_choice`, at least 16,000 context
tokens, and known input and output rates (which may both be zero). OpenRouter's
negative automatic-router rate sentinel is retained as unknown and excluded.
The “lowest-priced
eligible” shortlist estimates USD per million equivalent tokens as 70% input,
20% output, and 10% cached input. When no cached-input rate is published,
normal input pricing is used for that component. The consumer ranks raw rows
locally and does not alter the static model catalog or harness-owned picker
(DD-054). `clud models cheapest` is the user-facing text query;
`clud models cheapest --json` emits the same locally ranked rows for automation.

Each row also carries OpenRouter's reasoning vocabulary
(`supports_reasoning_effort` and the `reasoning` object); see
[Per-turn effort](#per-turn-effort-1528) for what it gates.

## Per-turn effort (#1528)

Claude Code re-reads the whole conversation when the effort level changes
mid-session **unless** its client-side, per-model capability `per_turn_effort`
is on. With it on, the change is delivered as a message appended after the last
user message — `{ "role": "system", "content": [], "output_config": { "effort":
"high" } }` — which lands after the cache breakpoint, so the cached prefix
survives. With it off, effort lives only in the request's top-level
`output_config.effort`, which is part of the cache key.

The harness resolves that capability from three places only: its baked model
catalog, gateway-served capability metadata, or the
`CLAUDE_CODE_MODEL_CAPABILITIES` environment variable. OpenRouter publishes
neither a wire field, a header, nor a discovery row for it, so the direct
`--openrouter` Anthropic-compat route (where clud is not in the request path at
all) can only supply it through the environment variable.

### Two names, and which one goes where

| Name | Owner | Meaning |
| --- | --- | --- |
| `supports_reasoning_effort` | OpenRouter's `reasoning_effort` token in `supported_parameters` | Published **gate**: this model accepts an effort setting at all |
| `per_turn_effort` | Claude Code's own capability vocabulary | The token clud **injects**; clud cannot rename it |

The mapping clud owns is OpenRouter-capability → harness-capability. The
artifact keeps OpenRouter's names verbatim, exactly as it does for
`supports_tools`, `supports_reasoning`, and `supports_vision`.

### The published fields (#1256's artifact)

Every row of `assets/openrouter-catalog.json` also carries:

- `supports_reasoning_effort` — `true` iff OpenRouter's `supported_parameters`
  contains `reasoning_effort`.
- `reasoning` — OpenRouter's own `reasoning` object, reduced to four keys under
  OpenRouter's names: `supported_efforts` (list of strings or `null`),
  `default_effort` (string or `null`), `default_enabled` (bool or `null`), and
  `mandatory` (bool or `null`). The producer always emits the object with all
  four keys — as explicit nulls when OpenRouter publishes nothing — so the
  document shape is stable. A published value of the wrong type fails the whole
  refresh rather than being coerced.

Both additions are additive, so `schema_version` stays `1`; the Rust consumer
carries `#[serde(default)]` on both, so a clud build older than the artifact,
and an artifact older than the field, still parse.

### No per-turn-*delivery* field exists

OpenRouter publishes **nothing** about per-turn delivery: `per_turn`,
`per-turn`, `mid_conv`, `mid-conv`, and `clear_at` return zero hits across the
whole published payload. The artifact therefore carries only the *gate* — "this
row accepts an effort setting". The claim that a mid-session change **keeps the
prompt cache** is a property of OpenRouter's `/api/v1/messages` endpoint, and
the live endpoint check owns it, not the artifact.

### The flip

`per_turn_effort: { "enabled": false }` is that switch, and it ships **dark**.
A recorded live endpoint check must confirm two things first (issue #1528, open
items 1 and 2): that the endpoint accepts the per-turn control, **and** that it
honors the level the control carries — a `200` alone does not prove the second.
Flipping `enabled` to `true` is what turns the injection on, and because it is
a served section it lands without a clud release.

- **Who reads it.** `server_settings::per_turn_effort_enabled()` /
  `per_turn_effort()`, called by `apply_anthropic_compat_overlay` in
  `foreground_runtime.rs`. When both the flip and the row's
  `supports_reasoning_effort` agree, the overlay injects
  `CLAUDE_CODE_MODEL_CAPABILITIES=<wire>=per_turn_effort` with `push_default`,
  so an ambient user value is preserved rather than overwritten, and that key
  is deliberately **not** in `ANTHROPIC_COMPAT_CONFLICTING` (which would scrub
  it).
- **Direct route only.** The gate is `descriptor.provider == OpenRouter`.
  Kimi's and DeepSeek's direct descriptors point at vendor endpoints that never
  see this message shape. Unified mode builds its own gateway environment and
  never reaches `apply_anthropic_compat_overlay`, so it is untouched.
- **Lazy and offline.** The catalog is read through
  `openrouter_catalog::catalog_cached_or_embedded()` — the daemon-state cache,
  then the always-present embedded copy, never a fetch — and only after the
  descriptor and flip gates pass. Launching costs no egress and no delay.

## Testing

- **Unit tests** never read the cache or the network. Under `cfg(test)` the
  process snapshot is always the built-in copy. The cache and refresh logic is
  tested through `store::load` with an injected fetch and a temporary cache
  path.
- **Subprocess test harnesses** set `CLUD_SERVER_SETTINGS=0`.
- **The embedded file** is checked through `include_str!`, never by reading the
  source tree, because CI runs tests from a prebuilt bundle.
- **The `model_contexts` producer** is tested by
  `tests/test_refresh_model_contexts.py`, which replays a recorded datasheet
  payload (`tests/fixtures/openrouter_models_sample.json`) through the
  normalizer and never touches the network.
- **The `per_turn_effort` producer** is tested by
  `tests/test_refresh_openrouter_catalog.py`, which feeds synthetic upstream
  rows through `normalize_row` for both the published and omitted shapes and
  asserts the wrong-type cases raise. The consumer's decode is covered by
  `openrouter_catalog.rs`'s unit tests against
  `tests/fixtures/openrouter_catalog_sample.json`.
- **The injection protocol** is covered by
  `tests/test_openrouter_per_turn_effort.py`, an opt-in
  (`CLUD_REAL_CLAUDE_TESTS=1`) fixture that drives the installed Claude Code
  binary against a loopback Anthropic-format stub and compares the captured
  turn-1 and turn-2 request bodies. It never leaves `127.0.0.1`.

## Code map

| File | Owns |
| --- | --- |
| `crates/clud-bin/assets/server-settings.json` | The built-in and served document |
| `crates/clud-bin/src/server_settings/mod.rs` | `Section`, `SectionSpec`, document parsing, merge, `Snapshot`, process loading, controls |
| `crates/clud-bin/src/server_settings/json.rs` | The strict JSON parser |
| `crates/clud-bin/src/server_settings/store.rs` | Cache, refresh thread, backoff, fetch |
| `crates/clud-bin/src/server_settings/sections.rs` | The section registry and the section types (DeepSeek, ModelContexts, PerTurnEffort) |
| `crates/clud-bin/assets/../../../ci/refresh_model_contexts.py` | The datasheet producer that rewrites the `model_contexts` section on a schedule (#1258) |
| `crates/clud-bin/assets/openrouter-catalog.json` | Embedded fallback and scheduled pricing catalog (#1256) |
| `crates/clud-bin/src/openrouter_catalog.rs` | Fixed-origin fetch, cache, fallback, and ranking (#1256) |
| `ci/refresh_openrouter_catalog.py` | Scheduled model and pricing producer (#1256) |

Consumers:

- `main.rs` reads the direct-launch DeepSeek default through
  `provider_default_model`.
- `foreground_runtime.rs` fills DeepSeek's haiku and subagent slots through
  `provider_subagent_model`, and emits `CLAUDE_CODE_MAX_CONTEXT_TOKENS`
  through `effective_context_window` for wire IDs the catalog does not know
  (#1258). It also reads `per_turn_effort_enabled` and the OpenRouter catalog
  row's `supports_reasoning_effort` to inject
  `CLAUDE_CODE_MODEL_CAPABILITIES` on a direct OpenRouter launch (#1528).

See [provider selection](provider-selection.md#served-deepseek-model-names).

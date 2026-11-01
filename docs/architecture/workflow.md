# Workflow Subsystem

`clud workflow <script.js> [--args JSON]` runs a workflow script written in
the Anthropic Claude Code workflow format, unchanged, and turns every
`agent()` call into a headless `clud -p` child (#2471).

## Script contract

```js
export const meta = { name: 'review', description: '...', phases: [{ title: 'Review' }] }
const r = await pipeline(items, x => agent(`review ${x}`, { label, phase, schema, model }))
return { r, mode: args.mode }
```

- `meta` must come first and be an object literal with `name` and
  `description`. `phases` is optional; when it is present, `phase(t)` and
  `agent(.., {phase})` must name a declared phase.
- Globals: `agent`, `parallel(thunks)`, `pipeline(items, ...stages)`,
  `phase(title)` and `args`. Top-level `await` and `return` work because the
  source is wrapped in an async function (`workflow/meta.rs::wrap_source`).

## Execution model

- **Engine.** The engine is embedded QuickJS (`rquickjs`). `agent()` returns
  a pending promise and queues an `AgentRequest`.
- **Host loop.** The host loop (`workflow/engine.rs::run_script`) drains
  QuickJS jobs and hands every queued request to the runner as one batch. It
  then resolves or rejects the promises and repeats until the script's
  promise settles. Calls issued together (for example by
  `parallel`/`pipeline`) therefore run concurrently. No async runtime is
  involved.
- **Runner.** The runner (`workflow/runner.rs`) runs
  `clud -p <prompt> --output-format stream-json --verbose [--model M]
  [--json-schema S]` per call through `running-process`, and drains the
  batch's children concurrently. A call's result is the `result` event's
  `structured_output` when a schema was given, otherwise its final `result`
  text. `is_error` or a missing result rejects the call's promise.

## TUI and artifacts

- `workflow/cli.rs::render_board` prints an ASCII per-agent board (id,
  label, phase, status, turns/cost or error) before and after each batch,
  with per-call stats from `stream_json::IterationSummary`.
- Each run writes `.clud/workflow/<script>-<unix>/result.json` and
  `agents.json`, plus a visible `WORKFLOW_DONE.md` at the git root that
  points at them, mirroring `LOOP_DONE.md` for `clud loop`.
- `--reuse` replays cached results (`.clud/workflow/cache/<script>.jsonl`, keyed by a hash of prompt, label, phase, model and schema; the request id is excluded) and re-runs only calls whose inputs changed. A run without `--reuse` truncates the cache. It is not named `--resume` because that flag belongs to the harness.
- `--dry-run` resolves schema calls with `{}` and text calls with `""` and
  prints each call's argv as JSON instead of launching agents.

## Tests

- Unit tests (`workflow/tests.rs`) cover batching, failures, phases, meta,
  the board, argv building and result parsing. They use a closure runner.
- Integration tests (`tests/integration/test_workflow.py`) drive real
  `clud -p` children against `mock-agent`, whose
  `MOCK_AGENT_WORKFLOW_RESPONSES` table scripts replies by prompt substring
  and records each call.

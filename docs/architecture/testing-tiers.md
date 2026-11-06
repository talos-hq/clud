# Testing Tiers

clud is tested at three layers. Pick the lowest one that can observe what you
are asserting.

| Tier | What is real | What is faked | Answers | Where |
|---|---|---|---|---|
| **1. Fake harness** | clud | the whole harness: `mock-agent` pretends to be `claude`/`codex` | does clud launch the harness correctly (argv, env, PTY, loop markers)? | `tests/integration/`, `crates/clud-bin/tests/` |
| **2. Gateway probe** | clud + Claude Code | the model API, as a passive recorder | what does Claude Code *send* (headers, effort, model IDs)? | `tests/test_real_claude_unified_effort.py` |
| **3. Real harness, mock agent backend** | clud + Claude Code + tools, git, hooks | the model API, as a **scripted driver** | what does Claude Code *do* with clud's skills, agents, workflows and hooks? | `tests/harness/` |

Tier 3 exists because Claude Code is where everything clud installs actually
runs: skills, agent types, workflows and hooks. The only way to prove that a
flow like `/do` or `/grind` behaves correctly is to run the real harness and
fake just the model. See
[DD-090](../DESIGN_DECISIONS.md#dd-090-test-claude-code-integrations-by-mocking-the-model-not-the-harness).

## Tier 3: how it works

```
test ─ Harness ─ claude -p … (real Claude Code, isolated config)
                    │  ANTHROPIC_BASE_URL
                    ▼
            mock-agent serve --script script.json   (scripted model)
                    │  records every request
                    ▼
       logs/requests.jsonl   logs/hooks.jsonl   fake gh state   git repo
```

- **`mock-agent serve`** (`testbins/mock-agent/src/serve.rs`) answers
  `POST /v1/messages` from a JSON script. It speaks SSE and JSON and binds to
  loopback only. The script format is in
  [`testbins/mock-agent/src/README.md`](../../testbins/mock-agent/src/README.md).
- **`Harness`** (`tests/harness/harness.py`) builds the isolated world:
  - `HOME`, and `CLAUDE_CONFIG_DIR` set to `home/.claude`, populated by
    `clud install-assets --home`, so the test runs exactly the skills, agents
    and workflows users get
  - a git repo on `main` with a bare `origin`
  - a fake `gh` backed by a JSON state file
  - a hook recorder chained before the real `clud-cmd-scan`
- **`Harness.run` vs `Harness.run_clud`:** `run` launches `claude -p`
  directly. `run_clud` launches the real `clud --openrouter -p` (#1984), so
  clud's own launch is under test: its `--settings` hooks (PreToolUse,
  `UserPromptSubmit`), its route env and a disposable daemon. The OpenRouter
  key comes from a test vault, and the debug-only
  `CLUD_TEST_DIRECT_UPSTREAM_URL` (with `CLUD_INTEGRATION_TESTS=1`) points the
  direct route at the scripted mock. `tool_results()` reads the JSONL stream's
  `tool_result` blocks. Example: `test_worktree_consent_e2e.py`.
  With `provider="--unified"`, every gateway upstream also points at the
  owned mock. `test_openrouter_zdr.py` invokes the real persistent setter,
  launches fresh native clients, and checks the logged upstream `provider`
  policy on both direct and unified requests, without paid traffic.
- **Interactive tool surfaces:** `Harness.run_clud_interactive` launches
  the reported model in an owned PTY with disposable onboarding/trust
  preferences. The loop plan-mode test uses this path because print mode
  omits `EnterPlanMode`. It verifies the actual hook, the denial delivered
  to the model, and a subsequent file write. The PTY and isolated daemon
  are closed even when a bounded expect fails.
- **Assertions** read three logs: the requests that reached the model, the
  hook calls (with each call's `agent_type`), and the world state (git refs,
  fake-GitHub PRs).

Run it with `CLUD_REAL_CLAUDE_TESTS=1 pytest tests/harness`; it needs an
installed Claude Code (`CLUD_REAL_CLAUDE` overrides the binary). In CI it runs
in full mode as `Test linux-x64 (harness)`, against the Claude Code version
pinned in `.github/workflows/_run-tests.yml`.
The directory collection hook applies `real_claude` to actual test items
before pytest's marker selection; a `pytestmark` variable in conftest alone
is insufficient. The required harness lane rejects exit 5, a missing or
invalid JUnit report, and a report containing no executed tests (including
an all-skipped suite). Optional unit/integration shards retain their existing
empty-selection behavior. The collection regression uses `--collect-only`
and never starts a model or harness process.

### Behavior the fixture relies on (Claude Code 2.1.306)

- Scripted tool calls from the backend really execute, headless via `claude -p`.
  `Agent` and `Workflow` are available.
- Subagents and workflow agents send their own system prompt and a narrower
  tool list, so the script can tell roles apart.
- A workflow agent's tool calls reach PreToolUse hooks with its `agent_type`.
- Native `parallel`/`pipeline` branches own independent resume cache chains.
  The older 2.1.282 pin shared one rolling chain across branches, so cached
  calls returning immediately could change later calls' keys during resume.
  Plain JavaScript `Promise.all` does not create native branch cache scopes;
  cache-reuse assertions cover the native helpers, not that unsupported route.
- A skill's `$ARGUMENTS` and `` !`cmd` `` are rendered at invocation.
  `UserPromptSubmit` can add context but cannot rewrite a prompt.
  `disable-model-invocation` hides skills from the model, but not agent types.

### Pitfalls

1. **The turn is not "is the last message a `tool_result`".** Claude Code
   appends reminder text after tool results, and background-agent completions
   arrive as extra user messages. The backend keys each step on the number of
   assistant turns already in the request, which makes every request
   self-describing.
2. **Project `.claude/settings.json` hooks don't fire under an isolated
   `CLAUDE_CONFIG_DIR`.** The fixture passes hooks with `--settings`.
3. **Say `python`, and keep clud's session shims off PATH.** The fixture
   strips `~/.clud/state/shims` and `~/.clud/state/rm-shim` so a developer's
   installed aliases cannot stand in for the build under test, and runs its
   own scripts with the test's interpreter.
4. **The fixture's `rm` is in a session.** A link to the built `clud`
   named `rm` is first on PATH, and the env carries the ABI stamp and alias
   directory (`tests/shim_env.py`), because the floor is session-only
   ([shim-dispatch.md](shim-dispatch.md)).
5. **Streaming is the default**, so the backend must speak SSE, including
   `input_json_delta` for tool input.

### Rendered skills (`!` lines), as pinned by the `/do` suite

`/do`'s body is one line, `` !`clud do-prompt "$ARGUMENTS"` ``, and the suite
pins how Claude Code treats it:

- **Permissions:** `allowed-tools: Bash(clud do-prompt:*)` lets it run with
  permission prompts on (H9).
- **Command guard:** the `!` expansion is not a tool call, so it never reaches
  PreToolUse or `clud-cmd-scan` (H10).
- **Failure:** if the command fails (for example, `clud` is not on PATH),
  Claude Code aborts the skill *before* any model call and shows the user
  `Shell command failed for pattern …`. The model never sees a half-rendered
  prompt (H11).
- **Codex** skills have no `!` lines. There the skill's fallback text tells the
  model to run `clud do-prompt <target>` itself, which makes it B tier.
- **Windows:** not yet covered; the harness job runs on Linux.

## Suites

- `tests/harness/test_smoke.py`: the framework itself (#1323).
- `tests/harness/test_do.py`: `/do` typed in Claude Code, scenarios H1–H12 (#1322).
- `tests/harness/test_grind.py`: the `grind-run` workflow with all five
  roles played per goal (#1324). It covers sequential and parallel runs, the
  concurrency caps (at most 4 plan/work/review agents, 1 integrator),
  dependency order, fix rounds and their cap, the worker shell caps, a dead
  planner, per-role procedures and tool lists, the internal-only agents, and
  the router's Docker and `ci.yml` text.
- `tests/harness/test_ask_answers.py`: the `AskUserQuestion` answerer
  (#1402). Print mode offers the tool only with a permission prompt tool, so
  `h.run(..., answers={...})` adds `answer_mcp.py` as one and registers
  `answer_hook.py`, which returns the answers as `updatedInput`. Runs
  without `answers` are never offered the tool.
- The `/grind` redesign suites (#1392), one per section of
  [grind.md](grind.md#tests), which maps each to what it covers:
  `test_grind_routing.py`, `test_grind_plan_only.py`,
  `test_grind_meta_of_metas.py`, `test_grind_overlap.py`,
  `test_grind_upfront.py`, `test_grind_prework.py`, `test_grind_stages.py`,
  `test_grind_feature.py`, `test_grind_problems.py`,
  `test_grind_reconcile.py`, `test_grind_scripts.py`,
  `test_grind_tracks.py`, `test_grind_review_gate.py` and the end-to-end
  scenarios in `test_grind_e2e.py`.

### OpenRouter context audit (#2225)

`tests/harness/test_openrouter_context.py` launches real `clud --openrouter`
with the DeepSeek wire ID and real Claude Code against `mock-agent serve`.
Eight scripted fixed-size tool outputs (including quotes, backslashes, tabs,
newlines and multibyte Unicode) must produce exactly nine requests.
Default and restricted-control profiles, a single injected HTTP 503 retry,
and native resume are audited without paid requests or ambient credentials.
Production tool access and project instructions are unchanged.

The audit validates exactly-once instructions, exact expected tool payloads,
assistant tool inputs/block types, serial call/result chronology, ordered user
text, exact output-to-call pairing, stable historical call/result identities,
unchanged schemas/system content, bounded per-turn growth, expected HTTP paths,
and absence of unrelated fixture canaries across the full request. Resume must
add one request and only the explicit new prompt/previous final answer. A retry
must have an identical received request-body fingerprint. The mock fingerprints
are equality checks within a run, not cryptographic security boundaries.

Copies of actual captures are corrupted adversarially: duplicate short/long
reminders or system sections, altered/lost/copied/padded output, extra model
calls, schema churn, unexpected assistant text/thinking/inputs, rewritten IDs,
extra message/result/cache metadata, unexpected images, changed result error
flags and unrelated context. The audit must reject every corruption. Unit controls
also verify received-byte measurement, UTF-8 handling, and canary detection in
system metadata, tool descriptions and other request fields, including escaped
string values and object keys. Positive controls preserve legitimate repeated
lines and escaped Unicode output rather than treating repetition itself as a bug.

`logs/context-audit.json` contains numeric request-section and cumulative body
byte counts. `context_bytes.request` measures the received HTTP body; section
sizes describe compact JSON. These are not provider token counts. Normal
append-only conversation history is resent by the Messages API, so cumulative
bytes differ from the size of a single request. Raw logs contain only the
isolated fixture world; headers/credentials are never logged. Optional
`audit_http` script logging records method, path and body size for all calls.

Passing this suite establishes the bounded scripted launcher/harness contract,
not whether each real model tool choice is useful, live billing correctness,
parallel tool batches, tool-failure recovery, automatic compaction behavior,
or the absence of every possible future context regression. The real provider
capture in #2225 complements the mock audit.

`tests/harness/test_openrouter_inventory.py` compares the compact bundled
descriptions of seven workflow-internal agents with their frozen verbose
descriptions. Full role prompts, tools, agent names, the orchestrator description
and project guidance stay intact. The A/B control owns its fixture copies so
startup refresh cannot erase it; the captured messages must be identical after
normalizing exactly those descriptions. It measures received-byte savings,
checks unchanged tool schemas/system content/catalog entries, and launches
`grind-editor` by name to verify that its full role instructions still load.

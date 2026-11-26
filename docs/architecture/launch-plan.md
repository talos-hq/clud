# Launch Plan

Every code path that decides "what would clud actually run" funnels through
`command::build_launch_plan_for_target` and consumes the resulting
`LaunchPlan`. The legacy `build_launch_plan` wrapper remains for native
provider/harness compatibility and focused tests; cross-route production code
must pass a resolved target. No other place in the binary reconstructs backend
argv, iteration budget, working directory, repeat schedule, DONE/BLOCKED
marker paths, or stream-json injection. `grind` is an exception to the loop
semantics described here: its required design is one interactive PTY prompt
seeded with the harness-native `/loop`; the harness, not `LaunchPlan`'s
external-loop fields, owns repetition. See [grind.md](grind.md). `grind`
requires the Claude harness and otherwise fails before launch.

Daemon-managed native sessions use the same construction path through the
typed `command::build_headless_turn_plan` helper. Its request carries only a
message, absolute cwd, and initial/resume identity — never raw argv or an
environment overlay. Claude headless turns add `--output-format stream-json
--verbose` plus `--session-id` or `--resume` before `-p`; Codex uses `exec
--json` or `exec resume --json`. The daemon adapter parses their JSONL
provider IDs separately from the foreground progress renderer.

## The struct

`LaunchPlan` lives in `crates/clud-bin/src/command/types.rs`. Trimmed shape:

```rust
pub struct LaunchPlan {
    pub command: Vec<String>,           // argv: command[0] is the backend exe
    pub iterations: u32,                // 1 for one-shot; >1 for `clud loop`
    pub backend: Backend,               // Claude | Codex | DeepSeek
    pub routing_mode: RoutingMode,      // Direct | Unified provider routing
    pub model_provider: Option<ModelProvider>,
    pub requested_harness: Option<HarnessSelection>,
    pub effective_harness: Option<Backend>,
    pub provider_source: Option<PreferenceSource>,
    pub harness_source: Option<PreferenceSource>,
    pub launch_mode: LaunchMode,        // Subprocess | Pty
    pub cwd: Option<String>,            // snapshot of std::env::current_dir()
    pub repeat_schedule: Option<RepeatSchedule>, // Some(interval_secs) iff --repeat
    pub task_summary: Option<String>,   // short label for session name
    pub loop_markers: Option<LoopMarkers>,       // DONE/BLOCKED absolute paths
    pub stream_json_progress: bool,     // claude subprocess-mode loop only
    pub codex_model: Option<String>,    // legacy bridge compatibility field
    pub model_selection: Option<ResolvedModelSelection>, // normalized provider/model/effort/context
    pub route: Option<ResolvedRoute>,   // every routing decision, resolved once (#1855)
}
```

`ResolvedRoute` (`crates/clud-bin/src/route_plan.rs`, #1855) is the one place a
launch's backend, upstreams, model slots, allowlist, discovery, context window,
effort capability, upstream routing, timeout and launch checks are decided.
Each backend renders it; none re-derives it. It is additive and omitted when
absent, so a pre-#1855 payload still round-trips.

`LoopMarkers { done_path, blocked_path }` and
`RepeatSchedule { interval_secs }` live in the same module. All three derive
`Serialize` / `Deserialize` so the complete plan round-trips through the
daemon's `WorkerLaunchSpec` (`crates/clud-bin/src/daemon/types.rs`).
`--dry-run` instead emits a stable, user-facing JSON projection built from the
same plan.

The provider/harness and model-selection fields are additive. Old optional
fields deserialize to `None`; `routing_mode` defaults to `Direct`.
`LaunchPlan::model_provider()` and `effective_harness()` fall back to the
legacy `backend`, preserving native-provider behavior.

The compatibility rule applies on every transport boundary: a newer client may
send these fields to an older daemon/worker only because omission remains a
valid native launch, and a newer daemon must preserve the legacy fallback when
it reads an old persisted spec. Repeat jobs pin resolved values in their argv;
they never re-resolve against a changed settings file. The cross-route ownership
and rollback contract is documented once in
[codex-via-claude.md](codex-via-claude.md).

## Construction pipeline

`build_launch_plan_for_target(args, target, backend_path) -> LaunchPlan` is the
production entrypoint in `crates/clud-bin/src/command/builder.rs`. In order, it:

1. **Seeds `cmd` with `backend_path`** and reads the effective harness from
   the resolved target.
2. **Adds Codex configuration before its subcommand.** Every configured `-c`
   override is emitted first, then `approvals_reviewer="auto_review"` (Codex
   auto review, #1847) unless the settings overrides or passthrough already
   choose a reviewer (`-c`/`--config approvals_reviewer=…`, `--approve-for-me`,
   `--not-so-yolo`), followed by the project-document fallback when the caller
   did not override it. Auto review only acts under `--safe`, because YOLO
   raises no approval requests ([DD-162](../DESIGN_DECISIONS.md#dd-162-codex-launches-default-to-auto-review-for-approvals)).
3. **Selects the Codex subcommand.** Explicit `-p` prompts and `loop` use
   `exec`; the built-ins `do`, `up`, `rebase`, and `fix` seed the interactive
   TUI without a subcommand. `grind` instead requires one normal Claude-harness
   PTY prompt seeded with `/loop`; see [grind.md](grind.md) for its separate
   contract.
   Continuation requests use `resume`; for interactive built-ins, `--last` or
   the explicit session ID is emitted before the generated prompt. Bare
   `--resume` is rejected for those built-ins because its session picker cannot
   share the first positional with a prompt. Claude has no corresponding
   sub-keyword.
4. **Adds common launch options.** The builder injects the harness-specific
   YOLO flag unless `--safe`, emits `--model`/`-m`, and appends Codex
   `resume --last` for `--continue`.
5. **Builds the selected task.** For `loop`, repeat duration is parsed first,
   then DONE/BLOCKED marker policy and paths are resolved, the task text is
   loaded, the marker contract is appended, and the prompt is pushed. `up`,
   `rebase`, and `fix` use their prompt builders. `grind` must only build and
   push its `/loop` seed, without loop markers or external repetition. A direct launch handles
   prompt/message/continue/resume arguments in harness-specific form.
6. **Forwards unknown flags** from `args.passthrough` after task-specific
   arguments.
7. **Resolves launch mode** from `--pty`/`--subprocess`, the effective harness,
   whether the launch is headless (Claude `-p`, `codex exec`, DeepSeek
   headless), loop state, and parent-TTY detection. Every interactive console
   launch is PTY on every platform; headless and redirected launches are
   subprocess (DD-086, #691).
8. **Injects stream-json progress flags** for Claude subprocess-mode loops.
   They are spliced immediately before `-p` so the prompt remains at
   `command[-1]`.
9. **Finalizes the plan** with provider/harness/source metadata, cwd,
   repeat/marker/task state, graphics settings, and stream-json state.

## Consumers

Every code path that runs (or describes) the resolved argv reads from a
`LaunchPlan`:

- `crates/clud-bin/src/main.rs` — `build_launch_plan_for_target` is called
  once after provider/harness resolution.
- `crates/clud-bin/src/main.rs` — `--dry-run` JSON emission (see contract
  below); exits 0 without spawning.
- `crates/clud-bin/src/runner.rs` (`run_plan_subprocess` and `run_plan_pty`) —
  per-iteration child
  spawn, reading `plan.command`, `plan.cwd`, `plan.iterations`, and
  `plan.stream_json_progress`.
- `crates/clud-bin/src/daemon/entry.rs` — `run_centralized_session` clones
  the plan into a `WorkerLaunchSpec` and ships it over IPC.
- `crates/clud-bin/src/daemon/worker.rs` — worker process
  re-spawns the backend using `spec.plan.command` and `spec.plan.cwd`.
- `crates/clud-bin/src/hook_health/prompts.rs` — `run_backend_prompt` carries
  the resolved launch target into `build_launch_plan_for_target` and runs the
  resulting argv as a one-shot subprocess for hook-migration prompting
  (`--fix-hooks`).
- `crates/clud-bin/src/loop_artifacts.rs` — `LoopSession::start` consumes
  `plan.iterations` to seed `TaskInfo::total_iterations` written to
  `<git-root>/.clud/loop/info.json`.

## Effective-harness-specific divergence

The legacy `Backend` enum now identifies the executable harness in plan
construction. Model-provider selection is carried separately by
`ResolvedLaunchTarget`.

| Concern | Claude harness | Codex harness | DeepSeek harness |
|---|---|---|---|
| Subcommand keyword | (none) | `exec` for `-p`/`loop`; none for interactive built-ins (including intended `grind`); `resume` for `-c`/`--resume` | `web` when interactive; `--profile headless` before a prompt |
| YOLO flag | `--dangerously-skip-permissions` | `--dangerously-bypass-approvals-and-sandbox` | none; DSH owns permissions |
| Approval reviewer | user | `-c approvals_reviewer="auto_review"` unless the user chose one | none |
| Model flag | `--model <id>` | `-m <id>` | OpenRouter: `--patch` overlay naming the model ([launch-targets](launch-targets.md#deepseek-harness-install-and-providers)); otherwise unsupported |
| Prompt delivery | `-p <prompt>` | bare positional | bare positional after the headless profile |
| `-m <message>` | `-m <message>` passthrough | dropped because it would clobber `--model` | rejected before bootstrap |
| `--continue` / `--resume` | native flags | `resume` subcommand | rejected before bootstrap |
| Stream-json progress | injected for subprocess-mode loops | not exposed; skipped | not exposed; skipped |

## YOLO injection

YOLO is on by default. The `--safe` flag is the opt-out — when set,
`build_launch_plan_for_target` skips the YOLO push entirely. This
matches DD-002 (yolo-by-default with explicit `--safe` override): every
clud-launched backend agent has permissions bypassed unless the user
explicitly asked otherwise. There is no per-subcommand override; the
decision is a single branch at the top of plan construction.

## Landing preference

Claude and Codex launches append a landing preference to their system or
developer instructions. By default it asks the agent to validate, push and
merge a PR when appropriate. `launch.land_bias` in the user settings accepts
`merge` (default), `pr` (stop after a validated PR), or `off`. The
`CLUD_LAND_BIAS` environment variable overrides the saved setting, and
`--no-land` overrides both. Unrecognized values and unreadable settings
disable the preference. `--dry-run` exposes the resulting instructions in
the command. This is guidance, not permission: user instructions, `--safe`,
repository rules, required reviews and branch protection still apply. The
instructions are appended to existing user instructions. The merge guidance is
shared with the rendered `/do` contract (`command/do_prompt.rs::LAND_CONTRACT`).
Bundled implementation skills hand off to that contract; issue-filing and
build-only tasks offer a next step without authorizing implementation. DeepSeek Harness
does not currently expose an equivalent instruction channel.

For implementation tasks in a repository with a remote, finish the delivery:
branch from the default branch when appropriate, commit, run the repository's
validation, push and open a PR referencing the issue (`Closes #N` for the
final fix). Both landing modes direct the agent to the bundled `clud-git`
skill. It verifies that the open PR holds the intended commit and returns a
clean checkout to the default branch before the wait, keeping the feature
branch available for fixes. Observe hosted checks with `github/pr_merge_watch.py`, fix failures,
and satisfy required review before submitting the PR through the repository's
merge mechanism. For a required merge queue, submit with
`github/pr_merge.py --match-head-commit <head-sha>` and see the PR through to
an actual merge; queue submission alone is incomplete. Who waits (the worker,
or a chaperone handoff at push or enqueue), when native labels are applied and
what each report means are owned by
[agent landing](ci.md#agent-landing-workers-end-at-enqueue-chaperones-own-the-merge).
Use the tools through
`clud tool run <tool> -- <arguments>` and their help for selectors and deadlines.
Start a bounded watcher once and wait on its live session or completion
notification; never write a polling loop. Report the merged PR URL, or the
specific approval, permission or policy gate that still prevents delivery.

Quick-fix may retain its permitted direct-push or immediate-merge path, but
must then observe the exact delivered commit's hosted run with
`github/run_watch.py` (or the PR watcher), report failures and fix forward in a
PR. This speed exception never bypasses required checks, reviews or queues.
Review-only skills return findings and a handoff, build-only skills point to
landing when implementation finishes, and issue-filing skills offer
`/do <issue-url>` without starting a fix. These pointers do not grant permission
for additional work. Cross-repository implementation lands upstream first,
releases or pins the merged dependency, then validates and lands the consumer.

## Unknown-flag passthrough

`args.passthrough` holds backend arguments the pre-split did not claim. In the
top-level flag region, clud first corrects exact `-deepseek` and exact public
option names with a Unicode dash prefix. An unrecognized flag-shaped token
is checked against clap's public top-level options: a near miss fails with a
suggestion, and any unknown option followed by a key-shaped token fails
closed without echoing the key. Other backend flags still pass through
byte-for-byte. The `--` separator is the explicit escape hatch and stops
correction and validation. A bare-word subcommand near miss is only a note,
because the word may be an intentional prompt. The builder appends accepted
passthrough after the synthesized prompt and before launch-mode splices.
Recognized Claude/Codex-only clud options are rejected for DeepSeek Harness;
native `dsh` options can still be passed after `--`.

Operational argv remains unredacted in memory for execution and web-terminal
forwarding. Launch records, dry-run JSON, crash reports, and whole-Args debug
rendering mask key-shaped values at their output boundaries. This does not
remove an inline key from shell history or the operating system process list;
vault-backed interactive entry is safer for a live key.

## `--dry-run` contract

`main.rs` emits this JSON shape and exits 0:

```json
{
  "command": ["claude", "--dangerously-skip-permissions", "-p", "..."],
  "iterations": 1,
  "backend": "claude",
  "model_provider": "claude",
  "requested_harness": "default",
  "effective_harness": "claude",
  "provider_source": "built_in_default",
  "harness_source": "built_in_default",
  "launch_mode": "subprocess",
  "repeat_interval_secs": null,
  "loop_markers": null
}
```

When a loop is active, `loop_markers` becomes `{"done_path": ..., "blocked_path": ...}`.
When `--repeat` is set, `repeat_interval_secs` is a positive integer.
Consumers: the Python integration suite under `tests/`, end-users debugging
their argv, and the hook-health remediator's preflight (it builds a plan,
inspects the command vector, and only then decides whether to spawn).
**Stability contract:** `command[-1]` is always the prompt body for prompt-
bearing invocations. The stream-json splice in `builder.rs` is the load-
bearing reason this invariant holds, and downstream tooling depends on it.

`launch_context` (Claude harness only, else `null`) previews the
[launch-context record](#launch-context-record-1675) this launch would
write, with `session: null` and `launched_at: 0`.

## Launch-context record (#1675)

A transcript does not record the child environment, so clud records the
context-window values the child actually receives, once per launch
(`crates/clud-bin/src/launch_context.rs`).

- **Written from the resolved env.** `ForegroundRuntime::start` builds the
  record after every route overlay has run, reading
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, `CLAUDE_CODE_AUTO_COMPACT_WINDOW` and
  `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` from the final child env. Sources come
  from a decision table over injected facts (`decide_max_context`,
  `decide_compact_window`) that mirrors each overlay: the direct route
  `push_default`s `server_settings::effective_context_window_with_source`
  (so an ambient value wins), the Codex bridge `set_env`s the common Codex
  catalog ceiling, the unified gateway and native Claude never touch the key.
  A child value the table did not predict is recorded with source `unknown`.
  `--dry-run` shows the table's prediction (`PlanFacts::preview`); a test
  asserts it equals the record built from a real runtime env.
- **Fields:** `v`, `session` (hash), `launched_at`, `clud_version`,
  `harness`, `harness_version` (always `null`: knowing it costs a spawn),
  `route` (`direct` / `unified_gateway` / `codex_bridge` / `native`),
  `provider`, `wire_model`, `max_context_tokens` and `auto_compact_window`
  as `{value, source}` with source `ambient` / `catalog` / `served` /
  `unset` / `unknown`, and `autocompact_pct_override`. No prompt, path,
  command text, credential, raw session id or other env value. Under 512
  bytes.
- **Two steps.** An interactive launch does not know its session id, so the
  launch writes `<state>/launch-context/pending-<token>.json` and passes the
  random token to the child as `CLUD_LAUNCH_CONTEXT_TOKEN`. The existing
  `clud session-hook --event SessionStart` reads it and copies the record to
  `<state>/launch-context/<hash>.json` with `session` set (`bind`).
- **Join key:** first 16 hex chars of
  `sha256("clud-launch-context-v1\0" + session_id)`, defined once in
  `launch_context::session_hash` and mirrored by
  `transcript_report.py::launch_context_key`; both test suites assert the
  same literal vector.
- **Retention:** pruned at every write: records older than 14 days are
  deleted, then only the newest 200 kept. One `read_dir`, no recursion.
- **Failure-silent:** writes are atomic (`fs_private::write_private_atomic`,
  owner-only); any error skips the record and the launch proceeds. No daemon
  round trip.

Why this shape: [DD-139](../DESIGN_DECISIONS.md#dd-139-the-launch-context-record-is-bound-by-the-sessionstart-hook-and-keyed-by-a-domain-separated-session-hash).

## Key types

- `LaunchPlan`, `LoopMarkers`, `RepeatSchedule` —
  `crates/clud-bin/src/command/types.rs`
- `ModelProvider`, `HarnessSelection`, `ResolvedLaunchTarget`, `LaunchMode`,
  `Backend` — `crates/clud-bin/src/backend.rs`
- `build_launch_plan_for_target` (production),
  `build_launch_plan` (native compatibility/test),
  `has_noninteractive_prompt`, `parse_repeat_interval` —
  `crates/clud-bin/src/command/builder.rs`
- `resolve_do_command_target` — `crates/clud-bin/src/command/do_input.rs`
- `push_prompt` — `crates/clud-bin/src/command/prompts.rs`
- `resolve_loop_task` — `crates/clud-bin/src/command/loop_task.rs`
- `WorkerLaunchSpec` (daemon wire-format wrapper) —
  `crates/clud-bin/src/daemon/types.rs`

## See also

- [loop-subsystem.md](loop-subsystem.md) — spec → plan → iteration → marker → artifact cycle.
- [grind.md](grind.md) — separate one-PTY `/loop` handoff contract; it does not use clud's external loop subsystem.
- [daemon-ipc.md](daemon-ipc.md) — how `WorkerLaunchSpec { plan, ... }` rides the wire.
- [`../../crates/clud-bin/src/command/README.md`](../../crates/clud-bin/src/command/README.md) — file-level map of the `command/` submodules.
- [`../DESIGN_DECISIONS.md`](../DESIGN_DECISIONS.md) — DD-002 (YOLO default + `--safe`), DD-005 (single source of truth for backend argv).

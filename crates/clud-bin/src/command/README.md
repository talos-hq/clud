# command/

Builds the `LaunchPlan` that downstream runners execute: effective-harness-specific argv assembly (`claude` vs `codex`), YOLO/safe-mode injection, subcommand-driven prompt construction (`loop`, `up`, `rebase`, `fix`, `do`, `grind`), `--repeat` schedule parsing, DONE/BLOCKED marker contract wiring for `clud loop`, and Claude `stream-json` progress injection for subprocess-mode loops.

The `LaunchPlan` contract (construction pipeline, consumers, `--dry-run` JSON) is documented at [docs/architecture/launch-plan.md](../../../../docs/architecture/launch-plan.md); the DONE/BLOCKED contract and `--repeat` no-overlap scheduler at [docs/architecture/loop-subsystem.md](../../../../docs/architecture/loop-subsystem.md).
The per-launch `unsafe_mode` field and its prompt behavior are specified in
[unsafe-mode.md](../../../../docs/architecture/unsafe-mode.md).
`grind` is distinct: its intended contract is one normal interactive PTY prompt
seeded with `/loop`, after which the harness owns repetition. It must not use
the clud loop marker contract, a headless launch, an iteration ceiling, or
external relaunching. It requires the Claude harness. See
[grind.md](../../../../docs/architecture/grind.md).
Provider/harness resolution happens before construction and is documented at
[docs/architecture/launch-targets.md](../../../../docs/architecture/launch-targets.md).
Provider-neutral model/effort/context normalization and direct-vs-unified
routing are documented at
[docs/architecture/provider-selection.md](../../../../docs/architecture/provider-selection.md).
The builder combines the session-only `--zdr` flag with the resolved saved
OpenRouter preference into `LaunchPlan.zdr`; every renderer consumes the same
policy. The dry-run DTO reports effective policy and provenance.

## Files

- `mod.rs` — module facade; re-exports the resolved-target production builder, the native compatibility builder, supporting helpers, and the `LaunchPlan` / `LoopMarkers` / `RepeatSchedule` types.
- `builder.rs` — core `build_launch_plan_for_target` orchestrator, the native `build_launch_plan` compatibility wrapper, typed daemon-headless turn plans, backend-aware interactive/headless prompt classification, and repeat/task helpers. It also builds the Claude-harness `/loop` seed for `grind`; the required one-PTY handoff is specified in [grind.md](../../../../docs/architecture/grind.md). Headless turns retain shared model/safety policy while producing Claude stream-json session argv or Codex `exec [resume] --json` argv. Also owns the `--disallowedTools` policy: `bridge_suppresses_plan_mode` strips `EnterPlanMode` on every Codex-provider / Claude-harness launch (the model can otherwise enter plan mode unprompted — see [DD-033](../../../../docs/DESIGN_DECISIONS.md#dd-033-plan-mode-is-disabled-unconditionally-on-the-codex-to-claude-bridge)), `--unattended` / `clud loop` additionally strip `AskUserQuestion`, and `plan_mode_suppression_notice` emits the green TTY-only override hint.
- `do_input.rs` — resolves `clud do`'s optional URL/free-form target before backend setup or spawn; prompts only on a foreground TTY and returns deterministic errors for dry-run, pipe, and background modes.
- `loop_task.rs` — resolves the `clud loop` positional (GH issue/PR URL, `#42` shortform, file path, or literal) into prompt text, with `gh`-backed cache under `.clud/loop/`.
- `prompts.rs` — static prompt templates (`FIX_PROMPT`, `GITHUB_FIX_TEMPLATE`, `REBASE_PROMPT`, `UP_PROMPT`) and the backend-aware `push_prompt`, `build_up_prompt`, `build_fix_prompt`, `build_do_prompt` builders. `build_do_prompt` wraps the prepared live procedure in `/goal`; missing preparation stops work. `build_grind_prompt` seeds `/grind <url>`.
- `do_prompt.rs` — owns `LAND_CONTRACT`, shared by default launches and the rendered `/do` procedure (see [landing preference](../../../../docs/architecture/launch-plan.md#landing-preference)); `clud do-prompt <target>` and the shared `render_current` launch renderer: the starting-branch verdict (`default`, `in_progress`, `stale_merged`, `stale_empty`, `dirty`, `unrelated`, `blocked`, `unknown`) from `git`/`gh`, plus the issue or free-form contract, a Read-based grind handoff for meta issues, or a stop. Tested end to end in `tests/harness/test_do.py`.
- `grind_scripts.rs` — `clud grind-scripts`, the renderer behind the `/grind` router's `` !`clud grind-scripts` `` line (#1336). It detects the repo root's lint and test entry scripts (`lint`, `lint.sh`, `lint.bat`, `lint.ps1` and the matching `test*`). It prefers `.bat`/`.ps1` on Windows and the extensionless or `.sh` form elsewhere, and prints each script's kind, path, run command (with `bash`/`cmd /c`/`pwsh -File` when it isn't executable) and the files to read for modes (the script plus the local files it delegates to). It never runs a script. The contract is in [grind.md](../../../../docs/architecture/grind.md#repository-linttest-scripts); tested in `tests/harness/test_grind_scripts.py`.
- `do_kind.rs` — classifies a `clud do` target before launch: a GitHub issue with open native sub-issues (`gh api …/sub_issues`) is a meta issue. It refuses when the question can't be answered; `CLUD_DO_KIND=single|meta` overrides it.
- `types.rs` — `LaunchPlan`, `LoopMarkers`, `RepeatSchedule` serde structs that flow into `--dry-run` JSON and into daemon job records; the plan carries additive `routing_mode` and normalized `model_selection` fields.
- `tests.rs` — unit tests covering yolo/safe, the bridge/plan-mode policy, and the shared helpers the `tests/` submodules pull in with `use super::*`. Per-domain suites live alongside in `tests/` — see [tests/README.md](tests/README.md).

## Key items

`LaunchPlan.allow_plan_mode` carries the explicit runtime-loop opt-out to
foreground and daemon workers. It does not override `--unattended`'s launch
deny. See [loop-scoped plan mode](../../../../docs/architecture/hook-dispatch.md#loop-scoped-plan-mode).

- `build_launch_plan_for_target(args, target, backend_path) -> LaunchPlan` — production path
- `build_launch_plan(args, backend, backend_path) -> LaunchPlan` — native compatibility/test wrapper
- `has_noninteractive_prompt(args, backend) -> bool`
- `interactive_builtin_resume_error(args, backend) -> Option<&str>`
- `resolve_do_command_target(args, tty_state, input, output) -> Result<(), String>`
- `parse_repeat_interval(raw) -> Result<u64, String>`
- `repeat_implies_no_done_warning(repeat, no_done, done) -> Option<&'static str>`
- `next_run_at_millis(completed_at_millis, interval_secs) -> u64`
- `summarize_task_name(input, max_chars) -> String`
- `resolve_loop_task(task, git_root, refresh) -> String` — `loop_task.rs`
- `resolve_marker_paths(cwd, git_root, done_override) -> MarkerPaths` — `loop_task.rs`
- `push_prompt(cmd, backend, prompt)` — `prompts.rs`
- `build_up_prompt(message, publish) -> String` — `prompts.rs`
- `build_fix_prompt(url) -> String` — `prompts.rs`
- `build_do_prompt(rendered) -> String` — `prompts.rs` (Codex seed: the whole contract as `/goal`)
- `build_claude_do_goal(target, prepared) -> String` — `prompts.rs` (Claude seed: a short `/goal` under the 4000-char cap; the contract rides in `--append-system-prompt`, #2372)
- `goal_compact::prepare_for_launch(target)` / `compact_goal(preamble, brief, lm)` — `goal_compact.rs` (#2522: a target over the cap is condensed by `claude -p --model $CLUD_GOAL_COMPACT_MODEL` (default `haiku`); the result is accepted only if every hard-constraint clause survives verbatim, else the verbatim constraint list is used, else launch fails quoting the clause that cannot fit. Brief and exercised goal are recorded under `~/.clud/state/do-goals/<session-id>/` (the checkout-claim session id; `CLUD_DO_GOAL_FILE` names its `goal.txt`), echoed to stderr and appended to the system prompt; `main::compact_do_goal` runs only after the checkout claim succeeds and never for `--dry-run`; `args.do_goal` carries it to the builder)
- `struct LaunchPlan` — executable argv plus provider/harness metadata, launch mode, repeat state, task summary, markers, and stream-json state
- `struct LoopMarkers { done_path, blocked_path }`
- `struct RepeatSchedule { interval_secs }`

## Used by

- `main.rs` — calls `build_launch_plan_for_target` and `repeat_implies_no_done_warning` to assemble the resolved plan and emit the `--repeat` warning before dispatch.
- `runner.rs` — consumes `LaunchPlan` to spawn PTY/subprocess and drive iteration loops.
- `loop_check.rs` — reads `plan.loop_markers` to poll DONE/BLOCKED after each iteration.
- `hook_health/prompts.rs` — builds a plan as part of doctor-style health probes.
- `daemon/entry.rs`, `daemon/types.rs` — persist and re-execute `LaunchPlan` records via the daemon worker.
- `loop_artifacts.rs` — shares the ISO-8601 format string with `loop_task::chrono_like_now`; both delegate the calendar arithmetic to `crate::civil_time` (#1206).

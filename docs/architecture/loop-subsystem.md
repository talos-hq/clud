# Loop Subsystem

This document owns the external `clud loop` subsystem only. Its marker files,
iteration budget, subprocess rendering, and runner relaunch lifecycle do not
apply to `clud grind`. The separate [`grind.md`](grind.md) contract requires
one interactive PTY session and delegates looping to the harness-native
`/loop` command.

`clud loop <task>` runs the backend agent repeatedly against the same task
until either (a) the agent writes a `DONE` or `BLOCKED` marker file under
`<git-root>/.clud/loop/`, (b) the agent emits a `<<<CLUD_LOOP_DONE: ...>>>`
fallback token on stdout (subprocess + Claude mode only), or (c) the
configured iteration budget is exhausted. The subsystem is split across
`command::builder` (plan + prompt assembly), `loop_spec` (task-spec
resolution and marker contract), `loop_artifacts` (durable on-disk artifacts),
`loop_check` (post-iteration polling), `stream_json` (subprocess progress
rendering for Claude), and `runner` (the actual per-iteration spawn loop).
The `--repeat <duration>` variant takes a different path: it disables
the marker contract entirely and hands the plan to the daemon worker
(`daemon/worker.rs`) for cron-style re-invocation.

`clud-loop`, the Codex-facing skill polyfill for Claude-style `/loop`, is
**retired**. `clud --codex --harness claude` (#622) runs Codex models under the
Claude harness, which has its own `/loop`, so the polyfill no longer earns its
place. It was also actively harmful on that route: its triggers key on the word
"Codex", so a Codex model driving the Claude harness selected the polyfill over
the harness's native `/loop`. Skill selection for a slash name is model-driven,
not deterministic expansion, which is why the same harness behaved differently
depending only on which model was reading the skill list.

Its name lives on in `skills::PURGED_BUNDLED_SKILLS`, which sweeps the installed
copies out of every backend's skills dir on launch. Dropping the
`BUNDLED_SKILLS` entry alone would only stop *new* installs — every user who
already had it would keep it forever. See [skill-system.md](skill-system.md).

A plain `clud --codex` session with no cross-route still has the external
`clud --codex loop` / `--repeat` process-runner path described above.

## Component map

- `command/builder.rs` — `build_launch_plan_for_target` builds the production
  plan from the resolved provider/harness target; `build_launch_plan` is its
  native compatibility/test wrapper. The builder injects the marker contract,
  parses `--repeat`, and decides launch mode.
- `command/loop_task.rs` — `resolve_loop_task` (line 29) classifies the
  positional, fetches/caches GH issues, returns prompt text.
- `command/types.rs` — `LaunchPlan` (line 6) with `loop_markers`,
  `repeat_schedule`, `stream_json_progress` fields.
- `loop_spec.rs` — `TaskSpec` (line 41), `classify` (line 80),
  `done_marker_contract`, `scan_completion_token` (line 557),
  `read_markers_or_token` (line 596).
- `loop_artifacts.rs` — `TaskInfo` (line 50), `LoopSession` driver,
  `ensure_loop_in_gitignore`, `materialize_working_copy`.
- `loop_check.rs` — `check_loop_markers` (line 13, PTY/file-only),
  `check_loop_markers_with_output` (line 24, subprocess + token scan),
  `loop_unconverged_exit` (line 64).
- `stream_json.rs` — `render_line` turns one Claude stream-json event into
  one human-readable progress line.
- `runner.rs` — `run_plan_subprocess` (line 109) and `run_plan_pty`
  (line 415): the actual iteration loops.
- `daemon/worker.rs` — `run_repeat_worker` (line 171) for the
  `--repeat`-as-cron path inside the daemon.

## Lifecycle (one iteration)

1. User runs `clud loop <task>` (optional `--loop-count N`, `--done <path>`,
   `--no-done`, `--refresh`, `--repeat <dur>`).
2. `args.rs` parses the subcommand; `main.rs` resolves the provider/harness and
   calls `command::build_launch_plan_for_target`.
3. `build_launch_plan_for_target` resolves git root
   (`loop_spec::git_root_from`),
   resolves marker paths (`resolve_marker_paths`), calls
   `resolve_loop_task` to materialize the prompt body, appends
   `done_marker_contract(done_abs, blocked_abs)` (skipped if `--no-done`
   or `--repeat` is set without an explicit `--done`).
4. For Claude in subprocess mode with `loop`, the builder splices
   `--output-format stream-json --verbose` in front of `-p` so the runner
   can render live progress (`builder.rs:200`).
5. `main.rs` constructs a `loop_artifacts::LoopSession`, calls
   `ensure_loop_in_gitignore` and `materialize_working_copy`, then hands
   the plan to `runner::run_plan_subprocess` or `runner::run_plan_pty`.
6. The runner clears stale markers, then per iteration `N` (1-indexed):
   - Logs `[clud] iteration N/total` to stderr.
   - `LoopSession::on_iteration_start(N)` updates `info.json` and writes
     `motivation.md` on `N >= 2`.
   - Spawns the backend (subprocess via `running-process-core`, or PTY
     via `NativePtyProcess`).
   - On subprocess + stream-json: drains stdout through
     `stream_json::render_line`, accumulating raw output for the token
     fallback (`runner.rs:349`).
   - On PTY: pumps stdin/stdout via `session::run_raw_pty_pump_*`.
   - `LoopSession::on_iteration_end(N, rc, err)` flushes `info.json`.
   - `check_loop_markers(_with_output)` polls the marker files (and, in
     subprocess mode, scans captured output for the token). On `DONE`
     returns `0`; on `BLOCKED` returns `3`; otherwise continues.
7. After the budget is exhausted, `loop_unconverged_exit` returns `2` and
   prints a diagnostic block listing the expected marker paths plus the
   actual contents of `.clud/loop/`.

## Task-spec resolution

`loop_spec::classify` (line 80) tries, in order: GH issue/PR URL
(`https://github.com/owner/repo/{issues,pull}/N`), short-form (`#42` or
`42`), local file path, otherwise literal prompt. GH URLs and short-forms
flow into `fetch_via_gh` (`gh issue/pr view --json ...`); short-form
additionally calls `gh repo view` to discover owner/repo. Results are
cached under `<git-root>/.clud/loop/<owner>__<repo>__{issue,pull}-N.md`
with a YAML frontmatter (`url`, `fetched_at`, `updated_at`). Subsequent
runs reuse the cache unless `--refresh` is passed. Literal prompts and
file paths are also mirrored under the loop dir by
`loop_artifacts::materialize_working_copy` (literal → `LOOP.md`, file →
`<original-filename>`), with skip-if-exists semantics so user edits
survive across iterations.

For large issue discussions, loop agents can use `clud tool run github/issue_context.py -- --repo OWNER/REPO --issue N` for a bounded overview and private, explicitly continuable snapshot. This tool is separate from the loop issue cache and `/grind` context pack; refreshing one does not update the others. See the [bundled tool guide](../../crates/clud-bin/assets/tools/README.md) for continuation and refresh.

## DONE/BLOCKED contract

`loop_spec::done_marker_contract(done_abs, blocked_abs)` returns a fixed
text block appended to the prompt body when marker injection is active.
It instructs the agent to write the **exact absolute** path on completion
(issue #95: relative paths led agents to invent `~/.loop/LOOP.md` and
similar). The default paths are `<git-root>/.clud/loop/DONE` and
`<git-root>/.clud/loop/BLOCKED`; `--done <path>` overrides DONE, and the
BLOCKED path is derived via `blocked_path_from_done` (`DONE.md` →
`BLOCKED.md`).

A PR accepted into a merge queue counts as complete: the contract tells the agent to write DONE with the PR URL as soon as the enqueue succeeds rather than waiting for the merge, so the loop moves on to the next item (#2471).

The contract also documents a token fallback for agents that cannot
write files: a line that starts with `<<<CLUD_LOOP_DONE:` and ends with
`>>>`. `scan_completion_token` (`loop_spec.rs:557`) parses captured
stdout for the LAST such token; the marker file always wins if both are
present. Token scanning is only wired in the subprocess path because PTY
mode never sees the child's bytes — they go straight to the user's
terminal.

After each iteration, the runner calls `loop_check::check_loop_markers`
(PTY) or `check_loop_markers_with_output` (subprocess). Marker poll is
synchronous and happens once per iteration boundary; there is no
in-flight watcher.

## Artifacts

Everything durable lives under `<git-root>/.clud/loop/`:

- `info.json` — `TaskInfo` (`loop_artifacts.rs:50`): `start_time`,
  `end_time`, `total_iterations`, `current_iteration`, `completed`,
  `error`, and a `Vec<IterationInfo>` with per-iteration
  `start_time`/`end_time`/`exit_code`/`error`. Flushed at
  iteration-start, iteration-end, and loop-end via `LoopSession`.
- `log.txt` — append-only text log of `=== loop start ===`,
  `=== iteration N start ===`, `=== iteration N end rc=X ===`, and
  `=== loop end <summary> ===` lines. Best-effort; never aborts on
  write failure.
- `motivation.md` — fixed prompt fragment written on iteration `>= 2`
  to anchor the agent in continuation mode.
- `LOOP.md` / `<task-file-name>` — working copy of the literal/file
  task spec; skip-if-exists.
- `<owner>__<repo>__{issue,pull}-N.md` — GH fetch cache.
- `DONE` / `BLOCKED` — marker files written by the agent (or, with
  `--done <path>`, wherever the user pointed).

`ensure_loop_in_gitignore` appends `.clud/loop` to an existing
`<git-root>/.gitignore` (and warns to stderr) if no covering entry is
already present. It does **not** create `.gitignore` if missing.

## stream-json rendering

Wired only when `backend == Claude && Command::Loop && launch_mode ==
Subprocess` (`builder.rs:200`). The builder splices
`--output-format stream-json --verbose` in front of `-p`, sets
`plan.stream_json_progress = true`, and the runner switches to
`run_with_stream_json_renderer` (`runner.rs:349`). Each captured stdout
line goes through `stream_json::render_line`, which returns one
human-readable line per `system/assistant/result` event (tool uses,
assistant prose, session start). Unknown events drop silently;
non-JSON lines pass through verbatim. The raw bytes are also
accumulated into `captured_output` so the token fallback still works.

PTY mode skips all of this — Claude already renders its own live TUI
into the user's terminal.

## --repeat scheduling

`parse_repeat_interval` (`builder.rs:250`) accepts `30s`, `5m`, `1h`,
`24h` (rejects zero, fractional, negative, compound, and unknown
units). When `--repeat` is set without `--done` and without
`--no-done`, `repeat_implies_no_done_warning` (`builder.rs:290`) prints
a warning and the DONE contract is omitted.

`next_run_at_millis(completed_at_millis, interval_secs)`
(`builder.rs:316`) computes the next fire time as
`completed_at + interval`, **not** `started_at + interval`. This is the
no-overlap invariant: if a run takes longer than the interval, the next
run is pushed out — runs serialize and never stack.

The actual scheduling loop lives in the daemon worker
(`daemon/worker.rs:171`, `run_repeat_worker`): per cycle it sets
`repeat_running = true`, calls `run_repeat_once`, then sets
`repeat_running = false` with `repeat_next_run_at` populated and sleeps
in 250 ms ticks until that wall-clock instant, checking each tick that
the daemon is still alive. Each `run_repeat_once`
(`daemon/worker.rs:253`) spawns the child with
`creationflags = invisible_helper_creationflags()` (no conhost popup on
Windows) and `Containment::Contained` (kill-on-close Job Object). Output
is captured into a TCP-broadcast log, not the user's terminal.

## Cache heartbeat (compat routes, #2046)

DeepSeek, OpenRouter and Kimi keep a request's prefix cache for about five
minutes, refreshed only by use, so a loop iteration that waits longer
re-enters cold. The unified gateway keeps it warm out of band
(`crates/clud-bin/src/cache_heartbeat.rs`):

- After each successful compat-route `/v1/messages` call it records the
  request as sent upstream, per conversation. Claude-route traffic is not
  touched.
- When a conversation has been idle for `ttl - 15 s` (285 s by default) with
  no request in flight, a background thread replays that body with one
  appended user turn, `just respond with ok`, `max_tokens` 8, no streaming
  and no thinking. System, tools and prior messages are byte-for-byte the
  recorded ones, so the cached prefix is what gets read. The reply is
  discarded.
- **Resume contract.** The heartbeat never enters the conversation: Claude
  Code, the loop, its markers, its iteration counters and `info.json` never
  see it, so the next real turn continues exactly where it stopped. A real
  request marks the conversation in flight, which blocks new heartbeats
  until it finishes; it then restarts the idle clock, so time in flight is
  never counted as idle. A real request may still start while a heartbeat is
  out; it is not delayed.
- At most 6 heartbeats per idle stretch (about 30 minutes), so an abandoned
  session stops paying. A failed heartbeat is logged and dropped; it can
  cost a warm cache, never a turn.
- Enabled by default. `NO_CACHE_HEARTBEAT` set to a truthy value disables
  it; unset or empty keeps it on. `CLUD_CACHE_TTL_SECS` overrides the 300 s
  TTL (values at or under 15 s are ignored).

## Key types

- `LoopMarkers { done_path: String, blocked_path: String }` —
  `command/types.rs:28`
- `RepeatSchedule { interval_secs: u64 }` — `command/types.rs:34`
- `LaunchPlan` (carries `loop_markers`, `repeat_schedule`,
  `stream_json_progress`) — `command/types.rs:6`
- `TaskSpec` enum (`GhIssue` / `ShortForm` / `File` / `Literal`) —
  `loop_spec.rs:41`
- `TaskInfo` + `IterationInfo` — `loop_artifacts.rs:50`,
  `loop_artifacts.rs:80`
- `LoopSession` (iteration-boundary driver) — `loop_artifacts.rs:168`
- `build_launch_plan_for_target` — production builder in
  `command/builder.rs`
- `build_launch_plan` — native compatibility/test wrapper in
  `command/builder.rs`
- `next_run_at_millis` (no-overlap scheduler) —
  `command/builder.rs:316`
- `scan_completion_token` (token fallback) — `loop_spec.rs:557`
- `read_markers_or_token` (file-wins-over-token resolver) —
  `loop_spec.rs:596`
- `check_loop_markers` / `check_loop_markers_with_output` /
  `loop_unconverged_exit` — `loop_check.rs:13`, `:24`, `:64`
- Iteration loop entry: `run_plan_subprocess` (`runner.rs:109`) and
  `run_plan_pty` (`runner.rs:415`)
- Repeat-mode entry: `run_repeat_worker` (`daemon/worker.rs:171`)

## Failure modes

- **GH fetch failure** — `fetch_and_cache_or_die`
  (`command/loop_task.rs:59`) prints `error: failed to fetch GH ...`
  and `std::process::exit(1)` from inside the launch-plan builder. No loop
  artifacts are created.
- **Short-form without `gh` repo context** — `resolve_current_repo`
  fails; same `exit(1)` path.
- **Marker file unwritable** — All `loop_artifacts` writes
  (`info.json`, `log.txt`, `motivation.md`, `.gitignore` append) are
  best-effort: IO errors are swallowed and the loop continues. The
  agent's own DONE/BLOCKED write going EACCES is invisible to clud —
  the loop will run to its iteration cap and exit `2` (unconverged).
- **Child crash mid-iteration** — runner records the non-zero exit
  via `LoopSession::on_iteration_end` and (if `plan.iterations > 1`)
  returns that exit code immediately rather than continuing. The
  next-iteration spawn is **not** attempted on a non-zero exit.
- **Ctrl+C mid-iteration** — `interrupted: &AtomicBool` is checked at
  the top of every iteration and inside both stdio drain loops. On
  trip: subprocess mode runs `teardown_interrupted_child` (Ctrl+Break,
  then `process_tree::kill_tree`, then `process.kill()`+2 s wait); PTY
  mode lets `session::run_raw_pty_pump_*` tear down. Final exit code
  is `130`, and `LoopSession::on_iteration_end(N, 130, Some("Interrupted by user"))`
  is recorded.
- **Repeat overlap** — structurally impossible: `run_repeat_worker`
  schedules off `completed_at`, not `started_at`. A slow run delays
  the next run; it cannot trigger a concurrent one.
- **Agent invents a completion filename** — `loop_unconverged_exit`
  (`loop_check.rs:64`) prints the expected absolute paths plus the
  actual contents of `.clud/loop/`, calling out stray `*.md` files
  that aren't `DONE.md`/`BLOCKED.md` so the user sees why the loop
  didn't converge.

## See also

- [`../../crates/clud-bin/src/command/README.md`](../../crates/clud-bin/src/command/README.md)
  — resolved-target plan construction, prompt assembly, `--repeat` parsing.
- [`../../crates/clud-bin/src/daemon/README.md`](../../crates/clud-bin/src/daemon/README.md)
  — daemon IPC, worker re-entry, repeat scheduling host.
- [`launch-plan.md`](launch-plan.md) — `LaunchPlan` as the single
  source of truth for what clud runs.
- [`daemon-ipc.md`](daemon-ipc.md) — TCP JSON protocol the repeat
  worker reports through.

## Restarting a finished loop (#2471)

When an interactive `clud loop` ends (stdin and stderr are terminals, not `--dry-run`, not interrupted), clud offers **Restart loop / Exit** through the shared selector (`loop_restart.rs`). Restart moves the run artifacts (`info.json`, `log.txt`, `NEXT_MESSAGE.md`, `motivation.md`, root `LOOP_DONE.md`, DONE/BLOCKED markers) into `.clud/loop/history/<millis>/`, keeps `loop.json` and `inbox/`, and re-runs the same argv.

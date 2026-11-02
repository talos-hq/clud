# Hook dispatch

How clud runs a repo's hooks itself, so they stop depending on where the agent
happens to be standing.

Design record: [#966](https://github.com/zackees/clud/issues/966) (spec),
[#967](https://github.com/zackees/clud/issues/967) (phased plan),
[#977](https://github.com/zackees/clud/issues/977) (settings compilation),
[#965](https://github.com/zackees/clud/issues/965) (the failure that started
it). Decisions: DD-047, DD-060, DD-061, DD-062, DD-063, DD-064.

## The problem

A `cd` in a Bash tool call moves the **session** cwd, not just that command's.
Every later tool call inherits it. Hooks are conventionally written as
repo-relative script paths — `uv run python ci/hooks/check-on-stop.py` — so
once the cwd drifts, they resolve against the wrong directory and fail. When
the failing hook is a blocking one, nothing can run: the session wedges until a
human intervenes.

Three things follow from that, and they are what this subsystem exists to fix:

1. Hooks must not depend on cwd.
2. A nested repo's own hooks never load at all — the harness reads hooks only
   from the session root.
3. The parent's hooks *do* keep firing inside a nested checkout, against files
   they know nothing about.

Upstream will not fix this: the cwd contract is documented as following the
agent, and the tracker's own reports disagree about whether cwd drifts or
silently resets (anthropics/claude-code#83636, #76708, #84685; the exact class
in #50960 and #42282 was closed NOT_PLANNED).

## Two layers

An explicit `clud --unsafe` launch bypasses Tier A while keeping Tier B; see
[unsafe-mode.md](unsafe-mode.md).

**Tier A — clud policy.** Built-in rules plus `bad_commands` / `bad_pipelines`,
evaluated in-process on every tool call. No trust gate: it is clud's own code,
and it self-roots per path via `discover_effective_clud_config`. This is what
`clud-cmd-scan` has always done.

**Tier B — the repo's declared hooks.** Whatever `.clud/hooks.json` declares,
run by clud rather than by the harness. This is the part that gets the rooting
contract.

Tier A runs first. A project guard runs last so its message is not buried under
one clud would have produced anyway.

## Loop-scoped plan mode

Native `/loop` can start after launch, so `--unattended` argv cannot cover
every scheduled session (#1844, DD-181). The compiled Claude settings route
`EnterPlanMode` to PreToolUse and successful `CronCreate`, `CronDelete` and
`ScheduleWakeup` calls to PostToolUse. A literal `/loop` prompt also marks
the session before its first scheduled call. Mentioning `/loop` in prose
does not mark it. The rule applies to every provider on the Claude harness,
including OpenRouter; ordinary interactive sessions keep plan mode.

Scheduling events update one daemon-owned session flag atomically. Each cron
ID is tracked independently; deleting one of several jobs keeps the guard
active. A successful dynamic-loop stop clears its marker. Native Stop
`session_crons` inventories reconcile expired or automatically removed jobs
when that field is supplied, and SessionEnd releases the marker. Failed
tool calls never establish scheduling activity. State is session-scoped and
in memory: daemon restarts forget it, as with other session flags. Older
harnesses without inventory fields retain activity until an explicit
delete/stop or session end.

`--allow-plan-mode` disables this runtime guard and survives serialized
daemon plans. A nested launch sets its own choice instead of inheriting the
parent's opt-out. The existing `--unattended` launch deny remains effective.
The hook leaves `AskUserQuestion` policy unchanged.

A hook cannot force Claude out of a permission mode the user already
selected. If `/loop` begins in manual plan mode, prompt context asks for
`ExitPlanMode`, with Shift+Tab as the user-controlled fallback. This is
recovery guidance, not a promise that plan approval is bypassed.

The native hook contracts and scheduler lifecycle are documented in the
[Claude hooks reference](https://code.claude.com/docs/en/hooks) and
[scheduled tasks](https://code.claude.com/docs/en/scheduled-tasks).

## Repeated Stop-hook continuations

A `/goal` condition the sandbox cannot satisfy keeps the harness's Stop
evaluator re-prompting (#1597). That loop belongs to Claude Code: `/goal` is a
prompt-based Stop hook, and the docs give no precedence between it and another
Stop hook's output, so clud does not try to end the turn. The harness has its
own breakers: an "impossible" verdict, a no-progress stop, and a cap of 8
consecutive blocks.

clud only detects the loop. When `stop_hook_active` is true and
`last_assistant_message` is identical on 2 consecutive Stop events,
`stop_repeat.rs` prints one `systemMessage` that tells the user how to break
it. A different answer or a user-driven stop resets the count. `SessionEnd`
drops the per-session state file under `~/.clud/state/stop-repeat/`.

## Declaring hooks

`<repo>/.clud/hooks.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "command": "uv run --frozen --no-sync python ci/hooks/check_cmd.py" }
    ],
    "Stop": [
      { "command": "uv run --frozen --no-sync python ci/hooks/check_on_stop.py", "timeout": 120 }
    ]
  }
}
```

- `command` is a **shell command line**, the same shape both frontends accept.
- `matcher` is a regex over the tool name, anchored so `Edit` cannot catch
  `MultiEdit`. Absent or `"*"` matches everything. A pattern that will not
  compile falls back to exact equality — a declaration clud cannot understand
  should under-match, never over-match.
- `timeout` is seconds, defaulting to 60.
- Event names are free-form strings, so a declaration for an event clud has not
  learned to fire sits inert rather than failing the file.

Parsing is lenient in the same way `repo_clud_config` is: one malformed entry
is skipped with a warning rather than taking the file's other hooks down with
it.

### Native hook dependency failures (#1739)

At launch, `hook_health` counts PreToolUse commands in each Claude or Codex
hook settings file that call `uv run` without `--no-sync` or
`--no-project`. It emits one warning per file. A new or unpublished dependency
pin in the working tree can otherwise make every hook invocation fail before
its script starts. Use `uv run --no-sync` when the environment is already
installed, or `--no-project` for a stdlib-only hook script. `--frozen` alone
prevents lockfile updates but still syncs the environment.

Claude Code itself formats failures from commands still declared in
`.claude/settings.json`: for a nonblocking exit without valid JSON, it shows
only the first stderr line and does not send the failure to the agent. Clud's
launch warning can prevent the dependency failure but cannot change that
renderer behavior or deduplicate its per-call notices. A hook that needs to
report a nonblocking diagnostic to the agent must return valid JSON with
`hookSpecificOutput.additionalContext` for its event. Moving a hook into
`.clud/hooks.json` gives clud control of execution, subject to the Tier B
contract below.

For hooks clud runs (`.clud/hooks.json`, Tier B), clud does deliver the
failure: a hook that cannot start, times out or exits non-2 still lets the
call through, and `clud_hooks_run` records a notice naming the command and
quoting the first and last stderr lines (uv's headline and its actual cause).
`block_bad_cmd_hook_notice` collapses identical notices into one counted
line, keeps only the messages this session has not been shown (a ledger
under the state dir's `hook-notices/`; a state error shows them rather than
hiding them), and adds the rest to the call's final output as
`additionalContext`, beside any decision that output already carries.

**Migrating means moving, not copying.** A command left in
`.claude/settings.json` *and* declared here runs twice, and the copy the
harness fires is the unrooted one — the exact failure the declaration was
supposed to fix. `hook_health` warns when it sees the same command in both.

### Tier B source: where a sub-repo's hooks come from

The parent's hooks come from its `.clud/hooks.json`. A `child` or `extern`
repo has no such opt-in — it is just a checkout the session wandered into — so
clud reads the files the frontend itself would run there, in order:

1. `.clud/hooks.json` — the opted-in declaration, preferred when present.
2. `.claude/settings.json` and `.claude/settings.local.json` —
   `hooks.<Event>` entries.
3. `.codex/hooks.json` — the same group shape, plus codex's root-level
   `<Event>` legacy shape.

Both the modern group shape (`{matcher, hooks: [{type: "command", command,
timeout}]}`) and the legacy direct shape (`{matcher, command, timeout}`)
parse; a `matcher` on the group is inherited by its handlers. Non-`command`
handler types are skipped, `hooks.state` (codex's own trust table) is never an
event, and entries dedupe across files by (event, matcher, command). A repo
declaring nothing in any of these files has no hooks — `None`, not an empty
set. See `clud_hooks::from_frontend_settings`.

## The execution contract

Every declared hook runs with:

- **cwd = the declaring repo's root**, whatever the session cwd is.
- **`CLUD_PROJECT_DIR` = that same root**, uniform across frontends.
  `$CLAUDE_PROJECT_DIR` is Claude-only and names the *session* root, so it
  cannot root a sub-repo's hook.
- **the harness's payload on stdin**, forwarded byte-for-byte, with the pipe
  closed afterwards so a hook blocking in `json.load(sys.stdin)` gets its EOF.

Exit codes mirror the harness's, because these are the same scripts users
already wrote for it:

| child exit | meaning |
| --- | --- |
| 0 | continue to the next hook |
| 2 | block: stop, and relay the child's own output as the reason |
| other | the hook is broken — warn, continue |
| timeout | warn, continue |

The last two rows fail **open** on purpose. A guard that cannot run is a bug in
the guard; turning it into a wall in front of every tool call is how a session
wedges, which is the outcome this whole subsystem exists to prevent. A blocking
hook's stdout is relayed verbatim rather than re-wrapped, since it may be
speaking the harness's own JSON protocol.

**Exception 1: removals on `PreToolUse`.** A hook that cannot parse its payload
has not inspected the command inside it, and for one class of command that is
unrecoverable. When `run_for_event` cannot decode its payload or recognize its
shape *and* the raw stdin bytes name a removal program, it denies rather than
allows — no configuration, in every session. This is the narrowest inversion
that covers the incident in #1064, where an unparseable payload silently allowed
an `rm -rf "$VAR"/` that expanded to `rm -rf /`. It applies to `PreToolUse`
alone: refusing is meaningless once the tool has already run, and the denial
speaks that event's protocol.

Note what is *not* a trigger. A read that stopped before EOF is recorded, but on
its own it means nothing is wrong — Claude Code routinely writes a complete
payload and leaves the pipe open (anthropics/claude-code#53177, and see
[windows-quirks.md](windows-quirks.md)), which is the very reason the idle
timeout exists. A genuinely truncated payload cuts a JSON string mid-flight and
so fails to decode, which the decode check already catches; the truncation
reason only sharpens the message. Treating the open pipe itself as unverifiable
denied every tool call whose text merely mentioned `rm`.

The probe (`raw_payload_mentions_removal`, in `block_bad_cmd_rm_vars.rs`) reads
raw bytes rather than extracted command text, and matches the removal as a word:
escape sequences collapse to separators, end-of-input counts as a boundary, and
a leading directory is stripped. Everything that is *not* a removal still fails
open, which is what keeps a hook hiccup from wedging the session. DD-057 records
why the inversion is scoped this narrowly and why the probe deliberately
over-matches.

**Exception 2: the command gate.** When `CLUD_CMD_GATE` is set to an enabling
value (DD-056 lists them) in the session environment, `run_for_event`'s own
allow-by-default exits — an empty payload, a payload that will not decode, a
payload shape it does not recognize — become denials instead. The gate's
guarantee is that every
command reaches a wrapper that can inspect its post-expansion argv, and a
payload the hook could not read is a command it could not verify. Allowing there
would restore exactly the silent-permissive default the gate exists to remove.
The inversion is scoped to that env var and to `PreToolUse`: an ungated session
keeps the fail-open behavior described above. See DD-056 and
`block_bad_cmd_gate.rs`.

**Exception 3: the recursive-delegation guard.** When
`CLUD_BLOCK_RECURSIVE_AGENTS` is set to an enabling value, `run_for_event`
refuses an `Agent` or `Workflow` call whose payload carries an `agent_id`. The
harness sets that field only for a call made from inside a subagent, so its
presence is the whole depth signal: the primary session keeps delegating, a
subagent may not. The check runs before the command rules — an `Agent` call has
no command for them to inspect — and it only fires if the hook is registered
with a `*` matcher, since a `Bash` matcher never delivers an `Agent` event.
Workflow-internal `agent()` calls emit no blocking event and stay uncovered.
See DD-069 and `recursive_agent_decision` in `block_bad_cmd.rs`.

## Which event an invocation serves, and who dispatches

**Stale-upstream guard (#1301).** For any caller, a shell command that runs
`git diff` against `@{upstream}` / `@{u}` is denied when `HEAD` does not descend
from that upstream commit (a rebase or force-push left the tracking ref stale, so
the range would include the whole rebase delta). The denial names the correct
`<merge-base>...HEAD` range against the default branch, its diffstat, and
`git/review_range.py`. A fresh upstream, a branch with no upstream, and any other
command are untouched. Code: `block_bad_cmd_stale_upstream.rs`.

### Hand-written wait guard (#1880)

Never poll. Wait on the existing process through the execution capabilities
the runtime actually exposes; see [watch recovery](#watch-recovery-and-continuation-2004).

For every shell call, from the main agent or any sub-agent (the rule reads
`tool_input.command` and never consults `agent_id`/`agent_type`), the scan
refuses: `while`/`until`/`select` loops and `for ((;;))`; a `for`/`foreach`
loop whose body sleeps; `tail -f`/`-F`/`--follow` and `Get-Content -Wait`
not wrapped in `timeout`; `watch`; a `sleep` in the same command as a `gh`
status read; and a standalone `sleep` over 60 s or of non-literal length
(DD-168). A `for` loop over a finite list with no `sleep`, a one-shot
`pgrep`/`gh` snapshot and a short `sleep` stay allowed. Process-list waits
(`while pgrep -f …`, `while ps … | grep …`) fall to the loop rule. The refusal
names the replacement: a persistent process session and its wait operation,
a supported background completion notification, or a waiter with its own
deadline (`clud ci`, `bosn ci run --wait`, `pr_merge_watch`). The only escape is the audited
`CLUD_BAD_CMD_OVERRIDE=hand-written-wait:<reason>`; the rule only tightens.
Code: `block_bad_cmd_wait_loop.rs`; fixtures from the issue's session in
`block_bad_cmd_wait_loop_tests.rs` (including a sub-agent payload). The PR-
specific polling rule (`blocking_pr_wait_reason`) now also covers counted
`for … sleep` loops, chained `sleep && gh …`, `gh api` run/job/check/Pages
reads and `gh pr view --json mergeStateStatus|mergeable|state`.

#1972 (DD-169) closes the laundering routes: wrapper programs (`timeout`,
`nohup`, `env`, `VAR=`, `stdbuf`, `nice`, `setsid`, `xargs`, `command`,
`exec`, …) are peeled before every check; `bash -c`/`eval`/`python -c`
payloads and heredocs fed to a shell or Python are scanned recursively;
heredocs written to a `.sh`/`.py` file and literal script files run from the
call's `cwd` are scanned for PR/CI waits; a `case`-arm `sleep` and a
`python -c` `time.sleep` count as sleeps; a native watcher inside any loop is
refused. PR/CI refusals spell out the canonical bounded waiter and conditional
session/background/foreground instructions, without inferring runtime tools
from the payload's tool name. The
helpers live in `block_bad_cmd_wait_scan.rs`; the audit's regression corpus
is `crates/clud-bin/tests/fixtures/wait_loop/corpus.json`, run by
`block_bad_cmd_wait_scan_tests.rs`. Codex sessions run it through the
session-flag hook clud injects at launch and, for a bare `codex`, the global
entry described in [Global Codex hook](#global-codex-hook-1972).

### Watch recovery and continuation (#2004)

A rejected native watch is recoverable when the supported replacement is
available. Start the existing waiter once, with an explicit repository and a
deadline below the execution tool's command cap:

```sh
clud tool run github/pr_merge_watch.py -- <PR> --repo OWNER/NAME --timeout 540
```

An Actions run with no PR (`workflow_dispatch`, `schedule`, `push`) has its own
waiter, `github/run_watch.py` (#1941). It takes the run id, fails fast on the first failed
job (`--cancel` also cancels the run), and exits 124 on a resumable timeout:

```sh
clud tool run github/run_watch.py -- <RUN_ID> --repo OWNER/NAME --timeout 540
```

The waiter reads the current run attempt in bounded 30-job pages and verifies
that the attempt did not change. A confirmed failed job can end the wait
immediately, while a green result requires complete, consistent job pages. If
three observations are unavailable or incomplete, exit 3 reports the last
known run state when available; re-run the same command to resume observation.
A watch timeout is an observation deadline, not evidence that the run stopped.
The terminal summary omits passing-job lines and points to the run for details.

Classify **declared execution capabilities**, independently of the tool name
in the hook payload. A normalized `Bash` call can originate from Codex; it
does not establish that Claude background-task flags exist.

| Available capability | Executable recovery |
| --- | --- |
| Persistent process session and session wait (this Codex environment) | Start with `exec_command`. If it returns a live session ID, call `write_stdin` with empty input and bounded waits on that same session until terminal exit. |
| Declared background-task and completion mechanism (supported Claude environments) | Start with `run_in_background` and wait for the exposed completion notification. Do not invent a notification tool. |
| Bounded foreground execution only | Run the waiter with a timeout below the verified outer command cap. Its terminal timeout remains incomplete; inspect the same CI and resume the waiter after exit. |
| Unknown or incomplete capabilities | Inspect tool declarations first. If necessary, use a harmless local process to verify creation of a live session, waiting on that same process, and terminal completion. An inconclusive probe does not establish support. Report any missing capability. |

A short local process such as `python -c "import time; time.sleep(2)"` can be
used with a shorter declared execution yield to probe session support. This
is a bounded capability probe, not GitHub polling. Prefer runtime declarations
when they are available. Never fabricate tools or replace a missing wait
operation with shell sleep/status loops.

A session ID or an observation deadline is not a terminal process result.
Keep using the same live handle; do not launch a second waiter while the
first still exists. A durable log or a lock file alone does not prove it is
live. The raw `exec_command` payload's `cmd`, declared `workdir` and optional
`shell` are interpreted by the shared hook; namespace qualification does not
change its command contract. Unsupported extra fields on `Bash` cannot change
its execution directory or dialect.

Select the requested **completion scope** too. By default the waiter uses
branch-required checks; without accessible protection it uses all reported
checks. `--require` overrides branch protection. Add `--require '.*'` when
all selected CI/acceptance lanes must finish, and `--no-cancel` when their
runs must remain intact. A green required-check result can coexist with a
pending advisory lane. It cannot substantiate “all workflows passed.” Keep
candidate and public-release runs distinct, even when check names overlap.

Read the terminal exit reason and the pinned head in the durable JSONL log:

| Result | Next action |
| --- | --- |
| `0`, reason `green` | Continue the already-authorized next step for the selected scope; verify the current head before merging. |
| `0`, reason `merged`, or `3`, closed | Reconcile PR state. Already merged does not prove every selected workflow passed. |
| `1`, failed checks | Diagnose the reported failure before changing code or retrying. |
| `2`, review activity | Read and address the relevant review before continuing. |
| `64`, usage | Correct the command; this is not a CI verdict. |
| `4`, watcher timeout | Work is incomplete. Revalidate the same CI run, then resume the canonical waiter after its process has exited. Do not restart CI solely because observation expired. |
| `124`, resumable tool watchdog | Inspect `status: in-progress` and `resume_hint`; after the wrapper has stopped the process, reuse the same arguments. Never interpret it as green. |
| Live handle or tool observation timeout | Wait on that same process; do not claim success, permanent blockage, or a terminal CI failure. |

The existing waiter retains its other explicit terminal results (approval,
conflict, unavailable GitHub, stale/missing checks, queued deadline and killed
process). Diagnose those results rather than collapsing them into green.

**Goal state is separate.** Neither a rejected command nor successful CI
changes a persisted goal's lifecycle. When the user removes a previous
blocker, use the runtime's supported goal-resume mechanism if one is exposed.
If no such tool is available, report that limitation and ask the user to
resume the goal; do not claim automatic continuation is active. Codex runtime
reactivation cannot be implemented by clud's hook or watcher. Already-authorized
ordinary work can continue when tools permit it without misreporting goal state.

Offline regressions use the real hook and bundled waiter with controlled
process sessions and GitHub snapshots. They exercise the published procedure;
they do not claim to execute a real Codex agent or reactivate its goal runtime.

### Repeated-call guard (#1674)

For every `PreToolUse` call that carries a `session_id`, the scan hashes the
tool name plus the canonical (key-sorted) `tool_input` and counts how many
times in a row that same call has arrived in the session. Call `N + 1` and
every further identical call are denied with a message telling the model to
stop repeating and decide afresh. Any different call resets the streak, and
so does a gap of more than 120 s since the previous call (a `/loop` tick or a
long `sleep` poll is not a burst). A denied call still counts, so a blind
retry stays denied.

- **Limit:** `N = 200` by default. `CLUD_REPEAT_CALL_LIMIT=<n>` in the
  environment wins, then `hooks.repeat_call_limit` in `~/.clud/settings.json`;
  `0` turns the guard off (the rollback switch). Evidence and tradeoff:
  [DD-140](../DESIGN_DECISIONS.md#dd-140-the-repeated-call-guard-denies-call-201-of-an-identical-streak-and-fails-open).

### Per-file shell read-hop guard (#2233)

The repeated-call guard above only sees an *identical* call; a hunt never
repeats one. Measured over 24 h of local transcripts, 55% of the bytes that
shell reads put into context came from file-sessions that read the **same
file** four or more times, and 562 file-sessions per day read one file eight
or more times. So `block_bad_cmd_hop.rs` counts hops per `(session, file)` for
shell reads (`sed`, `cat`, `head`, `tail`, `nl`, `less`, `bat`, `tac`, `wc`,
`awk`, `xxd`) and denies hop `N + 1` on that file with a message naming
`clud tool run code/outline.py`, so the next read can target one range.

- Narrow on purpose: only files that exist and are at least 8 KiB, only known
  source extensions, and `grep`/`git`/`gh` are searches, never hops. A file's
  count expires after 15 minutes without a hop, so a later revisit is fine.
- **Limit:** `N = 6`, per file. `CLUD_FILE_HOP_LIMIT=<n>` overrides it and `0`
  turns the guard off. `CLUD_ALLOW_FILE_HOPS=1` in a command allows one more
  slice deliberately (logged).
- **State:** `<clud state dir>/file-hop-guard/<hash>.json`, one file per
  session, at most 32 files per session, 2 days / 500 files retained. Fail
  open on every state error.

### Verification-to-edit ratio note (#1988)

`block_bad_cmd_verify_ratio.rs` classifies each PreToolUse call as an edit
(`Edit`/`Write`/`MultiEdit`/`NotebookEdit`/`apply_patch`, or a shell
`sed -i`, `git apply`, ...), a verification (clippy, dylint, cargo
build/check/test, pytest, ruff, `./lint`, `bosn ci run`, `clud ci`, ...) or
neither, and keeps the last 30 classified calls per session under the state
dir's `verify-ratio/`. When at least 10 of them are verifications and they
exceed `ratio x max(edits, 1)`, the allowed call carries one PreToolUse
`additionalContext` note telling the agent to scope its checks; it makes no
permission decision and never denies. After a note it stays quiet for 30
classified calls. `CLUD_VERIFY_EDIT_RATIO=<n>` wins, then
`hooks.verify_edit_ratio` in `~/.clud/settings.json`, default `3`; `0` turns
it off. It fails open like the repeat guard.

#2122 adds three signals over the same per-session ledger. They share the
note's quiet window.
- **Wall-time share:** each Bash PreToolUse records its start under its
  `tool_use_id`. The matching PostToolUse or PostToolUseFailure (same
  dispatcher, `Bash` matcher) adds the elapsed time to that call's kind
  (lint, build, test, ci, other). Once 10 minutes of Bash are measured, a
  verification share of at least `CLUD_VERIFY_WALL_SHARE` /
  `hooks.verify_wall_share` (default 70%, `0` off) notes once.
- **Modifiers:** three `--workspace`/`--all`/`--release` runs in a row, each
  after edits to a single package (the directory after `crates/`,
  `packages/` or `libs/`), note with `-p <package>`.
- **Escalation:** after any note, if verification still outnumbers edits
  past the ratio, the next workspace-wide lint (`bash lint`, a wide
  clippy/ruff, a whole-plan `bosn ci run`, `local-gate run`) is denied once
  per session. Repeating the command runs it.

`clud session stats [--session <id>]` prints the per-kind calls, wall time
and Bash share from the ledger. The id defaults to `CLAUDE_CODE_SESSION_ID`.
- **Override:** a command carrying `CLUD_ALLOW_REPEAT=1` skips the guard for
  that call and writes `REPEAT-GUARD-OVERRIDE` with the tool name to the hook
  log (`~/.clud/tools/hooks/block-bad-cmd.log`). `CLUD_ALLOW_ALL_CMDS=1`, in
  the env or the command, also skips it, like every other check. Non-shell
  tools have no command text, so their only switches are the limit settings.
- **State:** `<clud state dir>/repeat-guard/<hash>.json`, one file per
  session named by `sha256("clud-repeat-guard-v1\0session\0" + session_id)`,
  holding only the call hash, the count and a timestamp. Pruned to 2 days and
  500 files when a session's file is first created.
- **Fails open:** a missing session id, an unresolvable state dir, or a state
  read or write failure allows the call. It is a safety net against runaway
  loops, not a security boundary. Parallel calls in one response can race on
  the file; the last writer wins, which can only undercount.
- **Where it runs:** wherever clud's scan runs as a `PreToolUse` hook with a
  `*` matcher: Claude Code on every route (native, the direct OpenRouter /
  DeepSeek / Kimi overlay, the unified gateway) and Codex sessions whose hook
  payload includes `session_id`. A harness that runs no `PreToolUse` hook, or
  a `Bash`-only matcher, gets no guard (or a Bash-only one). A hook only sees
  a call after the model has emitted it, so the output tokens of a burst are
  already spent; the guard stops the results from re-entering context.
- Code: `block_bad_cmd_repeat.rs`, decision table in
  `block_bad_cmd_repeat_tests.rs`, end-to-end in
  `tests/test_repeat_call_guard.py`.

A bare `clud-cmd-scan` means `PreToolUse`. That is what every already-installed
hook line means, and those lines keep working untouched. Other events are named
explicitly: `clud-cmd-scan --event Stop`.

An already-installed line should register with `matcher: "*"`, not `"Bash"`. A
`Bash`-scoped matcher only ever fires on the Bash tool, so a catastrophic
command (`rm -rf $VAR/`) issued through Claude Code's PowerShell tool, an MCP
shell server (`mcp__*`), or a Codex session would bypass the guard entirely
(zackees/clud#1084). The scan self-selects the shell dialect internally, so a
non-shell tool call is a cheap no-op. clud's own launches do not depend on such
a line: Claude Code gets the compiled `GUARDED_TOOLS` matcher, and Codex
sessions get the session-flag `hooks.PreToolUse` override with `matcher="*"`
(`CODEX_HOOK_MATCHER` in `command/builder.rs`, #1954).

This repo dogfoods that: its committed `.claude/settings.json` and
`.codex/hooks.json` carry no scanner line of their own (#1954). A catch-all
line here once made every rule work for clud developers while users' launches
delivered `Bash` alone. `tests/test_hook_matcher_delivery_1954.py` checks
delivery through the compiled matcher alone, in a repo with no hook config.

Codex reports its own tool names even where a matcher reaches them through a
Claude alias: `apply_patch` (matched by `Edit`/`Write`) and `spawn_agent`
(matched by `Agent`). The rules name both, so the grind write cap, the grind
role-spawn cap and the recursive-agent guard hold on Codex too (#1954).

Two things can therefore invoke the dispatcher for PreToolUse — the bare line a
user installed, and the `--event PreToolUse` line clud compiles — and a declared
hook must run exactly once. The tie is broken by `CLUD_HOOK_DISPATCH`, which
clud sets in the session env whenever it registered compiled lines:

| invocation | Tier A | Tier B |
| --- | --- | --- |
| bare, no marker | yes | yes — nothing else can |
| bare, marker set | yes | no — the compiled line owns it |
| `--event <E>` | — | yes |

Sessions clud did not launch never see the marker and keep the old behavior.
This is why the invocation carries an `explicit` flag rather than just an event
name: a bare call and a compiled `--event PreToolUse` call name the same event
but play different roles.

## Delivery: compiled into CLI args, not written to files

clud never writes hook lines into a user's config. It **compiles** its hook set
into each frontend's native surface and passes it at launch:

- **Claude** takes `--settings <file-or-json>`, an *additional* source that
  merges with the settings files rather than replacing them, with hook entries
  concatenating across levels. `foreground_runtime.rs` already composes such a
  document into a session-lifetime tempfile and injects `--settings`, and
  already merges into a user-supplied `--settings` so neither shadows the
  other.
- **Codex** accepts `-c hooks.PreToolUse=...` plus `-c hooks.state=...` at
  launch. clud injects a `clud-cmd-scan` command hook and a trusted hash for
  its exact canonical JSON, without bypassing hook trust. It also injects
  generated deletion instructions through `developer_instructions`. Existing
  user hooks and instructions are preserved. The one exception to "never
  written to files" is the global scanner entry below.

### Global Codex hook (#1972)

The `-c` hook only guards Codex sessions clud launches. A bare `codex` in any
other repo ran unguarded (the #1972 audit counted ~1,300 Codex `exec` wait
loops), so every Codex global launch setup also runs
`codex_cmd_scan_install.rs` (action `codex-cmd-scan-install` in
`launch_setup.rs`, after `codex-hook-normalize`):

- `~/.codex/hooks.json` gets `{"matcher":"*","hooks":[{"type":"command","command":"clud-cmd-scan"}]}`
  appended to `hooks.PreToolUse`, unless any handler there already runs
  `clud-cmd-scan` (whatever its matcher). The file is created if missing.
- `~/.codex/config.toml` gets `[hooks.state."<hooks.json path>:pre_tool_use:G:H"]
  trusted_hash = "sha256:..."` for that handler, because Codex runs no file
  hook it has not trusted. An existing entry for the key is never overwritten.
- Read-modify-write under `~/.clud/settings.lock`, like `codex_hook_normalize`.
  Malformed JSON/TOML or an unexpected shape is logged (verbose) and left
  untouched; unknown fields and other hooks are preserved; nothing is
  rewritten when nothing changed. Codex's `[features] hooks` flag is not
  touched: current Codex runs hooks by default, which the `-c` path already
  relies on, and an explicit `hooks = false` is the user's choice.
- Opt out with `"hooks": {"codex_global_cmd_scan": false}` in
  `~/.clud/settings.json`; removing the entry by hand without that switch
  gets it reinstalled on the next launch.
- A clud-launched Codex session sees both hooks (both match `*` since
  #1954), so each guarded call is scanned twice. The verdicts agree; the
  repeated-call guard counts each call twice. The session hook still guards
  a user who opted out of the global entry.

Rationale: [DD-171](../DESIGN_DECISIONS.md#dd-171-codex-gets-a-global-clud-cmd-scan-hook-written-to-its-user-config).

This removes a whole hazard class that writing files would carry: idempotence,
read-modify-write lost updates, two writers fighting over one file (the #847
failure mode), per-repo gitignore assumptions, and stale state left behind by a
killed session.

Each harness's surface is recorded once, in `backend::SettingsSurface`
(`Backend::settings_surface`): Claude `--settings`, Codex `-c` overrides that
cannot carry hooks, DeepSeek `--profile` only. Call sites ask
`accepts_hooks()` rather than matching on the harness.

`--setting-sources` can also *subtract* a source, which would let clud absorb a
repo's existing `.claude/settings.json` hooks and fix repos with no migration
at all. Deliberately not done yet: excluding a source drops everything it
contributes, so a bug there costs the user `permissions`, not just hooks. See
#977.

### Hook health: `clud doctor`

`clud doctor` (`doctor.rs`, #1954) reports what a launch cannot see:

- alias freshness in clud's interpreter, session and helper alias dirs, and
  beside the running `clud`: stale (another build), missing (absent from a
  dir clud has populated) and retired names;
- files in those owned dirs that are not clud aliases. They are listed and
  never deleted, since clud does not own them; an installer's dot-prefixed
  staging copy may be another process's install in flight and is skipped;
- a foreign `clud-cmd-scan` first on PATH;
- scanner coverage per source: the fragment a clud launch passes, and every
  `.claude/settings.json` / `.codex/hooks.json` under the home and the
  current repo that registers the scanner, with the `GUARDED_TOOLS` each one
  misses (Codex aliases counted). Coverage is computed from the configs on
  disk; it describes what a harness would deliver, not proof that one ran.

`--json` prints the same report as one document. Exit 1 means an alias
finding; unknown files and coverage gaps are informational.

## Which repo's hooks fire

A session can touch files in more than one repo, and which repo's hooks apply
is decided by what the containing repo *is* to the session, not by path
geometry. Two questions have different answers per kind: do the **parent's**
hooks fire there, and do the repo's **own** hooks fire?

| kind | how it is registered | parent hooks fire there? | the repo's own hooks |
| --- | --- | --- | --- |
| `parent` | the session root | yes | yes, for parent- and child-owned paths |
| `extern` | immediate children of the repo's extern directory | **never** | its own Tier B source, rooted at the checkout, trust-gated (DD-060) |
| `child` | declared in `.clud/settings.json` | yes | its own Tier B source, rooted at the child, layered parent-first (DD-061) |
| unregistered | — | no | no |

`extern` roots are temporary, foreign visits: the parent's guards have no
business running against a repo it does not own and will not keep, and firing
them there is the #841 ENOENT wedge. A declared `child` is the opposite — part
of the parent's world, so the parent's guards apply to it.

Denial is layered and any deny wins: Tier A, then the parent's hooks, then the
child's own. A call that touches only child files still gets the parent's
guards; a call spanning roots fires each distinct root exactly once, parent
first; a call touching nothing any registered root owns fires no Tier-B hooks
at all. In a codex session (no `CLAUDE_PROJECT_DIR`), child and extern
execution is additionally gated behind codex's own project trust table in
`~/.codex/config.toml` — clud does not run a repo's hooks there before codex
itself would.

Since #986 those checkouts live **beside** the repo — `~/dev/myrepo` keeps them
in `~/dev/myrepo-extern/` — which makes containment a disjoint question rather
than a nested one: a path is under the repo or under its extern sibling, never
both. The legacy in-tree `.extern-repos/` is still recognized so existing
checkouts keep working. See `extern_root.rs` and DD-053.

Nested git repos are **not** auto-detected as children. Declaration is the
consent that makes the child tier's no-prompt trust sound, and that reasoning
collapses if nothing was declared.

### Containment comes from what the call names

Never from the payload cwd alone: a subagent editing
`.extern-repos/<sub>/src/lib.rs` usually still has the session cwd at the
parent root, so keying on cwd would answer "parent" for a file that is plainly
not the parent's. The resolution order is:

1. paths the tool names (`file_path`, `notebook_path`, `path`)
2. otherwise, for Bash, wherever the command would `cd` to — `cd
   .extern-repos/dep && make` does its work in the sub-repo, and cwd is only
   where it started
3. otherwise, cwd

A call that spans repos still gets the parent's guards for the parent's own
files: any touched path the parent owns is enough.

Roots clud resolves at launch that a hook cannot rediscover — `--add-dir`
targets and `permissions.additionalDirectories`, neither of which appears in a
hook payload — cross the process boundary in `CLUD_HOOK_ROOTS`, as JSON,
because a path-separated list is ambiguous on Windows where paths contain
`:`. The launch harvests them from its own argv and from the repo's
`.claude/settings*.json`, and registers them as `extern`: a granted sibling
directory is no more the parent's business than a checkout clud cloned, and
its project guards would misfire there. The two kinds of extern differ in
*trust*: a gc-tracked checkout's hooks stay off until the user names it with
`clud extern trust <name>`, while a root the user named at launch is the
consent itself and is never gated (DD-060). A launch that grants nothing sets
no variable at all.

### Trusting a foreign checkout

An extern checkout that declares hooks does not run them until it is trusted:

- `clud extern trust <name>` records `{name, origin}` in the parent's
  gitignored `.clud/settings.local.json` under `hook_trust.extern` (DD-062).
  The origin is read from the checkout's `.git/config`, so re-cloning the
  same name from a different origin does not carry trust.
- Until then, every dispatch that would have fired the checkout's hooks
  prints one visible notice — `[clud] extern checkout "dep" declares hooks,
  but is not trusted; they are not running. Trust it with: clud extern trust
  dep` — and keeps the hooks off. Opted-in `.clud/hooks.json` declarations in
  an extern are gated exactly like frontend-settings ones: the trust boundary
  is the checkout, not the file format.
- `clud extern trust --list` and `--revoke` round-trip the store.

## Where the code is

| file | role |
| --- | --- |
| `clud_hooks.rs` | `.clud/hooks.json` schema + Tier B source: the frontend settings files a sub-repo's hooks come from; discovery, matcher semantics |
| `clud_hook_roots.rs` | the typed root registry, containment, and the firing rule |
| `hook_trust.rs` | the `hook_trust.extern` allowlist in `.clud/settings.local.json`: load, `is_trusted` (name + origin), `record`, `revoke`, in-process `.git/config` origin read |
| `extern_cli.rs` | `clud extern trust` / `--list` / `--revoke` |
| `hook_health/codex_trust.rs` | `codex_project_trusted`: the `~/.codex/config.toml` project trust table that gates child/extern Tier-B execution in codex sessions |
| `clud_hooks_compile.rs` | compiling declarations into a frontend's native registration, and the merge that keeps it from displacing anything |
| `foreground_runtime.rs` | composing the launch-scoped `--settings` document and injecting it into argv |
| `clud_hooks_run.rs` | Tier-B execution: rooting, stdin payload, exit-code contract |
| `block_bad_cmd.rs` | the hook binary: event arg, Tier A, then Tier B — the firing matrix (per-target roots, containment), the codex and extern-trust gates, layered deny |
| `block_bad_cmd_cd.rs` | `bash.block_cd` cwd pinning and the three-level `"auto"` resolver, the hook-command scanner both it and `hook_health` use, and the `CwdChanged` drift predicate |
| `block_bad_cmd_cwd_changed.rs` | the `CwdChanged` backstop handler: the drift warning and Tier-B dispatch for a session-cwd move, with the deny downgraded to a warning |
| `backend_bootstrap.rs` | the `claude --version` capability probe (`probe_claude_cwd_changed_support`, floor 2.1.83) that decides whether the backstop line is registered |
| `hook_health/inspect.rs` | warnings: broken `git rev-parse` prefix, double-declared hooks |

## Phase 5: `"auto"` relaxation and the `CwdChanged` backstop

`bash.block_cd: "auto"` is a three-level resolver decided by the repo's
migration state (D13, DD-063):

| resolved policy | when |
| --- | --- |
| strict | any cwd-sensitive raw hook is in scope — `.claude/settings*.json`, `.codex/hooks.json`, or the user's home copies. Phase 1 behavior, and the right one: the harness fires those hooks unrooted, so any cwd drift breaks them. |
| relaxed | the repo is fully dispatcher-managed — a `.clud/hooks.json` opt-in with no sensitive raw hooks left. `cd` moves freely **within** the registered trees (parent, children, extern roots — the Phase 3 `CLUD_HOOK_ROOTS` set, not just the parent root), and only a `cd` that escapes all of them is denied. |
| off | no hooks at all, or no repo. No behavior change. |

The `.clud/hooks.json` opt-in marks the scan `dispatcher_managed` even when
the raw frontend configs are empty, so migrating earns relaxation; its
declared command text never counts toward sensitivity, because the
dispatcher roots every declared hook (cwd + `CLUD_PROJECT_DIR` = the
declaring repo's root, D10). A sensitive raw hook keeps the repo strict even
after migration, and an empty `hooks.json` is not an opt-in.

**Repeated refusals are condensed** (#2314). The first `block_cd` denial in a
session carries the full text: the registered roots, the settings key and the
override. Later denials in the same session are one line that still names the
rule id, the `(cd DIR && CMD)` escape and
`CLUD_BAD_CMD_OVERRIDE=block-cd:<reason>`. The "already shown" marker is a file
under the clud state dir (`block-cd-seen/<hash of session id>.seen`), pruned
like `block_bad_cmd_repeat`'s state. It fails open to the full text when there
is no session id or the state cannot be read or written. Condensing changes
only the message, never what is denied. A subagent has its own session, so it
sees the full text once (`block_bad_cmd_cd.rs` `condense_at`).

**A blocked `cd DIR && CMD` runs in a subshell instead** (#2354, part of the
refusal-round-trip work). When `block_cd` would deny a POSIX command whose very
first command is a top-level `cd`, the hook returns `updatedInput` with the whole
command wrapped as `(cd DIR && CMD)` plus an `additionalContext` notice
(`[clud] ran (...) instead of ...`), so the call is not lost. The decision
follows the payload's `permission_mode`: `allow` under `bypassPermissions`
(a hook `ask` would raise the permission dialog even there and stall headless
agents), `ask` in every other mode (the rewrite vets only the `cd`, so the rest
of the command keeps the harness's normal decision, as in #1087), and the old
deny when the mode is missing. Codex keeps the deny until its handling of
`updatedInput` for this rewrite is verified (it may ignore the rewrite and run
the original `cd`); a Codex session is detected by the absence of
`CLAUDE_PROJECT_DIR`. It declines
and keeps the deny (with the #2314 condensing) when the wrap could change
meaning: a parse error, a top-level `exit`/`exec`/`export`/`set`/`source`/`.`/
`alias`/`unset`/`pushd`/`popd` (and similar session builtins), a top-level `&`,
a function definition, a script variable assignment, a here-doc, `#` comment,
backtick, newline or trailing backslash, or when another rewrite already applies
to the call (`block_bad_cmd_cd.rs` `subshell_substitute`).

**The `CwdChanged` backstop** (D12, DD-064) closes the gap the scanner
cannot see: a session cwd moved by an alias or a script is invisible to
PreToolUse, yet breaks exactly what pinning protects. clud registers its own
`clud-cmd-scan --event CwdChanged` line alongside the declared events and
the handler:

1. resolves `bash.block_cd` against the session parent root and warns when
   the new cwd violates the policy — `[clud] CwdChanged: the session cwd
   moved to …` — pointing the user at `cd` back or at the escape hatches;
2. dispatches repo-declared `CwdChanged` hooks rooted at the repo the
   session now stands in, via the same firing matrix as every other event.

Hygiene only, never correctness: the handler always exits 0 (the harness
gives this event no decision control — an exit 2 only shows stderr, the
change is not reverted), a declared hook's exit-2 is downgraded to a
warning, and the line is registered only where a capability probe says the
installed client fires the event — a per-launch bounded `claude --version`
against the 2.1.83 floor, degrading silently to no line on any failure. The
line rides only on opted-in repos, so a non-opted-in launch keeps its
pre-Phase-2 argv exactly.

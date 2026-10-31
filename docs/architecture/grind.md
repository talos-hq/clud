# `grind` Execution Contract

This document is the single owner of the required behavior of `clud grind`
and the `/grind` skill DAG it launches. Current behavior that conflicts with
it is a bug, not a compatibility contract.

## Launch

`clud grind` acquires the current checkout's mutation claim before the
harness starts; direct `/grind` invocations acquire it in the router. See
[checkout claims](checkout-claims.md) for contention and restart behavior.

`clud grind [url]` starts exactly one normal, foreground, interactive PTY
session for the Claude harness. With no URL, clud resolves the repository's
`origin` remote to its forge issues page; an explicit URL is used verbatim.
The seeded prompt is:

```text
/grind <resolved URL>
```

clud passes it to the harness's normal interactive entrypoint. It does not
use a headless prompt flag or subcommand (`-p`, `exec`), relaunch the backend,
issue an external repeat prompt, inject or poll DONE/BLOCKED markers, set an
iteration count, use the daemon repeat worker, or enable stream-json
rendering. Everything after the prompt belongs to the harness.

`grind` requires the Claude harness: `/grind` drives Claude Code's Workflow
tool and, in cron mode, its native `/loop`. A Codex or DeepSeek model uses
`--harness claude`. Other harnesses are refused before launch
(`command::builder::grind_launch_error`); clud never substitutes `clud loop`
or a hand-rolled loop.

### Third-party model routes and the Workflow tool

`/grind` depends entirely on Claude Code's Workflow tool. `--openrouter`,
`--deepseek`, `--kimi` and the other Anthropic-compatible providers resolve
to the Claude harness through `ModelProvider::native_harness()`, so
`grind_launch_error` (`crates/clud-bin/src/command/builder.rs`) admits them
([provider-selection.md](provider-selection.md)). The router starts the
workflow and ends its turn; the run then advances only on task
notifications. If the gateway model never emits the Workflow tool call, or
the tool is suppressed, no notification ever arrives and the run waits
silently with no error. Signs of this failure:

- the router's turn ended with no Workflow `tool_use` in the transcript;
- `~/.clud/tmp/grind/<session>.json` gets no run-facts update and no
  worktree appears under `~/.clud/worktrees/`;
- `--disallowedTools` in `clud --dry-run ... grind <url>` names Workflow.

The remedy is to rerun on the Claude route or on a model that supports tool
use. `grind_launches_the_workflow_session_on_every_non_anthropic_claude_route`
in `crates/clud-bin/src/command/tests/grind_launch.rs` pins the launch side.

**Verified end to end on OpenRouter (#1811).** On 2026-10-05, installed
clud 2.8.27 ran `clud --openrouter --model ~anthropic/claude-sonnet-latest
grind` on smoke meta #1825 (one child, #1818). The run used sequential mode,
the session model for every role, and `feature_merge: later`. Every stage
completed: routing, the plan-only pass, the two-call question round,
prework, plan → work → review → integrate → land (goal PR #1827 merged into
the feature branch by the lander), and Finish. Finish marked feature PR #1826
ready, filed follow-up #1828, and removed the run facts and `plan.json`.
#1826 then merged, closing #1818 and #1825.

Why every role billed to OpenRouter: the launch was pinned (the
`gateway model discovery is off` notice), and the overlay gave every
`ANTHROPIC_DEFAULT_*` slot and the subagent slot the pinned id. Its only
credential was the OpenRouter token at `https://openrouter.ai/api`, with
`ANTHROPIC_API_KEY` empty. Workflow agents with no model override inherit
that session, so no request could authenticate to Anthropic.

What this run did not check (#1811):
- OpenRouter's activity page, so the per-role wire ids were not read there.
- The invalid-key failure path. Testing it would overwrite the stored vault
  key.

Defects it found: #1828 (`./test` pty timing on the integrator host), and
#1830 (classification flipped bug/feature for the same child across two
launches).

## Repository identity and bounded watch recovery (#1897)

Intake resolves a GitHub `OWNER/REPO` from an explicit input URL or the
checkout's `origin` remote. A fork's gh default can select its upstream, so
every repository-scoped gh command and PR waiter receives an explicit
`--repo`; API endpoints spell out that owner/name. An unresolved or
conflicting identity stops before mutations or delegation.

| Workflow field | Meaning |
| --- | --- |
| `repo` | Absolute local checkout path |
| `github_repo` | Explicit GitHub `OWNER/REPO`, including plan-only calls |
| `plan.repo` | The recorded forge identity; supplies `github_repo` when omitted |

`grind-run` validates the slug and rejects missing, malformed or conflicting
values before any agent starts. Comparison is case-insensitive. Every
classification, prework, goal-role and final feature-landing prompt carries
the selected GitHub repository separately from its checkout. The feature
landing commands carry the same `--repo` explicitly.

Planner/lander reads use the supported `gh issue view --repo OWNER/REPO N
--comments` or `--json comments` forms. This does not grant general `gh api`
access to capped roles. The task parser already understood the separator,
but the top-level splitter still diverted its file list into backend
passthrough; `grind-facts` now owns its raw `--` arguments end to end.
The task separator (`grind-facts task --checkout ABS -- FILE...`), scoped gh
reads and prework's quoted parentheses are covered through the real CLI/hook
and Rust parser/role-cap tests.
Prework still comments only on the recorded meta issue; other roles retain
their existing mutation limits.

The lander follows the shared [watch-recovery contract](hook-dispatch.md#watch-recovery-and-continuation-2004).
Choose a bounded deadline from verified execution limits and the slowest
selected CI lane, including setup and queue time. Keep one live waiter and
use the declared session/background completion mechanism. Required-check
green and all-reported-check green have distinct scopes; grind's normal
all-check wait uses `--require '.*'`.

Exit `4`, or resumable wrapper `124`, leaves CI incomplete. Reconcile the
same repository, PR, head, run IDs and scope, then continue after the old
waiter has stopped. Never restart live waiters or CI because an observation
expired. Observation count alone neither spends a fix round nor proves a
failure. A verified external runtime limit must be reported with the
incomplete state and an exact resume command.

Offline tests execute the production workflow body in a controlled JavaScript
host, exercise real hook/CLI paths, and run the existing watcher with offline
snapshots. They do not claim a real Claude Workflow or model integration.

## The DAG

`/grind` is a router over bundled skills, one bundled workflow, and eight
capped agent types:

```
/grind ─ reconcile ─ /grind-intake (routing) ─ grind-run planOnly (classify) ─ preflight ─ ONE question round ─┐
  ┌─────────────────────────────────────────────────────────────────────────────────────────────────────────┘
  ├─ parallel   ─┐
  ├─ sequential ─┴─ workflow grind-run: prework → [bug stage, then feature stage]: plan → work → review → integrate → land ─ Finish
  └─ cron ─ /grind-cron ─ /loop: one sequential run per tick
```

The sections below follow that order: [routing](#input-routing), the
[planning pass](#plan-only-classification-pass) and its threshold,
[meta of metas](#meta-of-metas-and-no-overlap),
[preflight and the question round](#preflight-and-the-single-question-round),
[problem reporting](#problem-reporting),
[prework](#prework-and-the-plan-comment), the
[two stages](#bug-stage-then-feature-stage) and
[feature-branch mode](#feature-branch-mode).

| Asset | Source | Installed to |
|---|---|---|
| Skills `grind`, `grind-intake`, `grind-prework`, `grind-plan`, `grind-work`, `grind-review`, `grind-integrate`, `grind-land`, `grind-cron` | `crates/clud-bin/assets/skills/` (`skills.rs::BUNDLED_SKILLS`) | `~/.claude/skills/`, `~/.codex/skills/` |
| Agents `grind-orchestrator`, `grind-gate`, `grind-prework`, `grind-planner`, `grind-worker`, `grind-reviewer`, `grind-integrator`, `grind-lander`, `grind-chaperone` | `crates/clud-bin/assets/agents/` (`claude_files.rs`) | `~/.claude/agents/` |
| Workflow `grind-run` | `crates/clud-bin/assets/workflows/grind-run.js` (`claude_files.rs`) | `~/.claude/workflows/` |

Each workflow agent runs as its `grind-<role>` type and is told to invoke
its leaf skill, so the procedure lives once, in the skill, and each leaf is
also usable directly (for example `/grind-land` on an existing PR).

The read-only `grind-orchestrator` role is the router-callable exception to
the internal-role delegation rule. It inspects all repositories and proposes
the run graph; it cannot write, fetch, build or create checkouts, even in
parallel mode. Its shell cap rejects output redirections and command-local
environment overrides in addition to mutating commands. The router calls it
once after intake and presents the graph for approval before classification.
The approved graph travels as `orchestration`, separately from classification
stages. Its selected goals run in topological order, and approved merge
dependencies settle before downstream planning, even when a per-goal planner
omits them. Each dependent role receives its approved blockers' settled
outcomes, including their reported PR URL and optional release artifacts
(`version`, `tag`, `release_url`). Reported artifacts do not satisfy a
publication gate. Failed upstream goals block their dependents; independent
goals can continue.

Cross-stage resumption and separately rooted repository sessions remain under
development in #1338 and #1422. The current graph path returns an explicit
blocked result for missing-stage dependencies or a sister repository needing
its own session. Unavailable publication evidence defers that branch, as
described below; neither condition counts as a successful merge or authorizes
execution in the wrong root.

The internal `grind-gate` role defaults to Haiku and denies shell execution without a binding. Its hook can
bind an exact read-only native Bash check from run facts' approved `orchestration` graph to
a native workflow task labeled `gate:<downstream-id>:<blocker-index>`. Lookup
uses the hook's parent-session transcript path and agent ID, streams the
session-local workflow directories with constant memory, and rejects ambiguous metadata,
symlinks, oversized metadata, wrong roots/roles/repositories and plan-only
facts. The label cannot select a command outside that approved blocker.
Lookup time grows with retained workflow directories, without a fixed limit
on how many workflow runs a recurring session may accumulate.
This relies on harness-owned metadata and router-owned run facts; it is not a
sandbox against arbitrary processes that can modify those control files.
The pre-tool hook claims one check attempt per native gate agent. It wraps the
approved read-only Bash expression with a per-attempt completion receipt that
preserves its shell status. This is necessary because Bash may interpret a
nonzero predicate status as a successful tool call and omit that status from
its hook response. Native success/failure hooks require the matching zero-status
receipt before recording success in the locked session-local sidecar.
Explicit background requests are denied; timeout/background responses,
interrupted checks and absent or unrecognized receipts fail closed. A `StructuredOutput`
report of `ready: true` requires a successful outcome matching the current
approved check, label and root. Missing or pending evidence cannot release
work. A retry needs a new native gate agent. These records share the harness
session directory's lifecycle and trust boundary; they contain the binding
and outcome, without command stdout/stderr.
The workflow starts this role after the approved upstream goal lands and
before downstream planning. A successful native check releases that blocker;
an unavailable or unsuccessful observation defers the dependent and its
transitive dependents, preserving the original gate and immediate blocker.
Independent work continues; an upstream implementation failure remains a
failure rather than a deferred publication branch. Each observation uses a
fresh native agent, with bounded native timer backoff between observations;
independent goals continue while a gate waits. The result includes a small
`grind-publication-resume/v1` manifest for precisely the deferred goals.
Per-goal observation epochs change gate prompts only on deferred branches.
The workflow replays whole earlier observation rounds unchanged before starting
the next round for deferred goals: native cache lookup stops globally after
the first new agent call, including calls on other parallel branches. This
ordering applies in sequential mode too. At most 64 resume generations are
accepted; an exhausted history stops explicitly and retains the deferred work.
Full-graph verification is ongoing. The router retains the
original approved arguments and actual native launch references in run facts
for same-session `/loop` resumption. Native resumption and scheduled handoff
are undergoing harness verification; cross-repository execution remains under
development.

Graph resumption requires Claude Code 2.1.306 or newer: the old 2.1.282 runtime
used a shared rolling cache chain whose keys changed when parallel cached
calls completed in a different order. The PreToolUse hook checks the latest
matching session-version record in a bounded tail of the harness transcript.
Missing metadata receives a bounded 310 ms native flush window because hook
groups can run before asynchronous transcript persistence finishes. A known
old version is refused immediately; absent metadata never grants admission.
An older, missing or unrecognized version refuses graph launch or resumption.
Native PostToolUse launch receipts also bind each cache to its originating
supported runtime, session, run ID and script path. A resume must also match the
completed native workflow snapshot: preserve every original argument and use
exactly its returned next epoch map. Missing snapshots, exhausted manifests or
model-invented epoch advances fail closed. A runtime upgrade does not
migrate an old cache; missing or incompatible provenance refuses resumption.
Keep the deferred manifest and launch references, report the unsupported
capability, and do not run Finish or restart completed goals. The check
uses the same harness-owned transcript trust boundary as native gate identity.

### Grind skills require explicit invocation

The leaves and roles exist for the workflow, and for a person testing one by
hand; a model must never pick them up from an ordinary prompt.

- **All grind skills**, including `/grind`, have
  `disable-model-invocation: true` for Claude and Codex's
  `allow_implicit_invocation: false` policy. A person can still invoke them
  directly. The router reads `grind-intake` and `grind-cron` as files next to
  its own directory instead of invoking them.
- **The agent types** ignore that frontmatter, so they're still listed to the
  model. clud's PreToolUse hook refuses direct `Agent` calls to the internal
  execution roles. `grind-orchestrator` is the read-only router exception.
  The workflow's `agent()` spawns don't go
  through the `Agent` tool, so the workflow is unaffected.
  `CLUD_ALLOW_GRIND_AGENTS=1` lifts the block for testing a role by hand.
- **Each execution agent carries its procedure.** A hidden skill can't be loaded with
  the `Skill` tool or `skills:` preload, so `claude_files.rs` writes each
  agent file with its leaf skill's body appended. The skill remains the
  single source. The standalone orchestrator instructions live in its agent
  definition; it has no duplicate leaf skill.

### Router questions

A workflow cannot ask questions while it runs, so the `/grind` skill asks
everything first:

- **Mode.** *Parallel*: each goal gets a git worktree; workers only read and
  write. *Sequential*: one goal at a time from `origin/<main>` in the local
  checkout, no worktrees or sister clones, so a heavy C++/Rust build cache is
  reused. *Cron*: the harness's `/loop`, one issue per tick, each tick a
  sequential run. Parallel Rust worktrees keep private Cargo targets if builds
  or tests may overlap. The measured, opt-in shared-target option for strictly
  sequential work is in [skill-system.md](skill-system.md#worktree-cargo-targets-1685).
- **Models** for planner, worker, reviewer and integrator (the lander shares
  the integrator's). The default is the session's own model, whatever route
  resolved it; an unchanged default is omitted so the agent inherits it.
  `grind-run.js` maps `args.models` onto each `agent()`'s `model`. On a
  direct gateway route the launch is pinned (DD-077) and every
  `ANTHROPIC_DEFAULT_*` slot carries the pinned id, so a tier alias stays
  inside the pin; a full wire id passes straight to the gateway, outside
  it, because the direct route has no bridge to refuse it (#1810).
- **Local CI**: offered only when `docker info` succeeds and
  `.github/workflows/ci.yml` exists. Otherwise the router prints why
  ("Docker/github actions disabled due to no docker running", or that
  `ci.yml` is missing) and CI is off. The choice is local-only (`act` on
  this machine): remote CI always runs, and grind only adds testing (#1429).
  `block_bad_cmd_grind_caps.rs` refuses, for every grind role, a skip marker
  (`[skip ci]`, `[ci skip]`, `[no ci]`, `[skip actions]`, `[actions skip]`,
  `skip-checks: true`) in a commit, PR or merge message, `gh pr edit
  --remove-label` of a CI-lane label, and `gh workflow disable`; adding a
  lane label stays allowed.

These are part of the single up-front round described in
[Preflight and the single question round](#preflight-and-the-single-question-round).
The router records the answers (`mode`, `ci`, `scripts`, `preflight`,
`feature_merge`, `problem_reporting`, `tracks`, `meta`, and later `feature`)
in its **run facts** for the hook below, and removes them when the run ends.

**Run facts (#1337).** One JSON file per Claude Code session,
`~/.clud/tmp/grind/<session_id>.json`, never in the working tree. The
router gets the path from `clud grind-facts path` (it reads
`CLAUDE_CODE_SESSION_ID`, which Claude Code exports to shell commands) and
removes the file with `clud grind-facts clear`. The hook reads the file for
the `session_id` in each PreToolUse payload; a workflow agent's payload
carries its parent session's id (`tests/harness/test_grind_facts.py`), so
every agent resolves its own run. Two runs, in one repo or two, never share
or clobber facts, and one run finishing cannot change another's caps. A
session with no file, or with a file older than 72 hours (a crashed run the
session-temp sweep has not removed yet; every hook lookup and
`clud grind-facts path` refresh a live one, and `/grind-cron` asks for it
every tick), gets the strictest caps
(sequential, no CI), and the hook log says why
(`crates/clud-bin/src/grind_facts.rs`,
[DD-103](../DESIGN_DECISIONS.md#dd-103-grind-run-facts-are-keyed-by-session-id-and-live-under-cludtmpgrind)).

**Goals in several repositories (#1422).** The router starts one
`grind-run` per repository, concurrently, all under its one session id. The
facts file then holds `{"repos": [{"repo": "<abs checkout>", ...}, ...]}`,
and the hook selects the entry whose `repo` contains the calling agent's
working directory (the deepest when roots nest); `clud grind-facts task`
files a task under the entry containing its checkout. A directory in no
entry, or in two of equal depth, gets the strictest caps, never another
repository's `scripts`, CI or feature. One workflow per repository, rather
than one workflow over several, because `grind-run` holds one `repo`, one
`main`, one integrator semaphore and one repository's caps.

### Input routing

Issue [#1404](https://github.com/zackees/clud/issues/1404); the steps live
in [`grind-intake/SKILL.md`](../../crates/clud-bin/assets/skills/grind-intake/SKILL.md).
Every run works on a meta issue:

- **One issue.** The bundled `github/is_meta_issue.py` tool alone decides
  meta-ness. A meta issue's open children are the goals. A multi-part
  non-meta issue gets intake's one question, *Convert #N into a meta
  issue?* (*Convert to a meta issue* / *Abort*): convert creates the
  children first, then a meta issue that `Tracks #N`, and comments on the
  original (which stays open); *Abort* refuses with
  ``run `/do N` (or `clud do N`) instead`` and creates nothing. A
  single-change issue is wrapped in a one-child meta (`Tracks #N`, #N
  attached as its only sub-issue) and runs the normal bug stage (#2160).
- **Issue list.** A new meta issue tracks the given issues as sub-issues.
- **Prompt.** Split into deliverables, one child issue each, under a new meta.
- **Nothing.** Pick from the open issues, then route as a list.
- **Stops that create nothing.** A meta issue whose children are all closed
  (`Nothing to do`), a `gh` failure reported by the tool (never read as "not
  meta"), and a conversion that fails partway (the partial state is listed)
  all end the run before any question, the run facts or worktree.

**Tracks.** After the plan-only pass each child carries a track, `bug`
(lands on `<main>` in the bug stage) or `feature` (lands on the feature
branch), recorded in the run facts' `tracks`; see
[Bug stage, then feature stage](#bug-stage-then-feature-stage).

### Plan-only classification pass

Issue [#1406](https://github.com/zackees/clud/issues/1406). Before asking
anything about a meta issue, the router writes `{"phase": "plan"}` to
the run facts and starts `grind-run` with `planOnly: true, meta`. One
`grind-planner` classifies every child as a bug (`#N bug → main`) or a
feature (`#N feature → grind/meta-<T>-<group>`), names feature groups and
their independence, and returns without starting any other role. The hook
refuses the planner `git worktree add` and file writes in this phase.
The complexity threshold is
`grind-run.js::thresholdVerdict`: it picks `path: regroup` only for a confident
classification with every child placed, 8+ children, and at least 2
independent feature groups of 3+; otherwise it logs
`keeping #T as is: <reason>` and the meta issue is left untouched. A child
the planner leaves out is printed `#N unclassified` and counts as unplaced.
The router then rewrites the run facts without `phase` for the real run. The
`phase` marker in the run facts is the hook's only plan-only signal; the
`PLAN-ONLY` prompt text is for the planner, and the hook never reads it.

### Meta of metas and no overlap

Issue [#1412](https://github.com/zackees/clud/issues/1412). The commands
live in the router skill, section 2b
([`grind/SKILL.md`](../../crates/clud-bin/assets/skills/grind/SKILL.md));
this is the contract.

- **Regroup only after confirmation.** `path: regroup` is a proposal until
  the user confirms it in the question round.
- **Shape.** One sub-meta per feature group; bugs stay directly under the
  top meta. A single feature group gets no sub-meta.
- **User-made sub-metas** are reused in place: the old body is kept in
  `<details>Previous content</details>`, surplus groups get new sub-metas,
  and leftovers are rewritten as `no children after regrouping`.
- **Grind-made sub-metas** (marker `<!-- grind:v1 -->`)
  are kept and their children are not touched.
- **Moves** use `replace_parent`. The 8-level depth / 100-children guard
  fails the run before prework, not midway.
- **Undo.** Every change is recorded in the run facts' `undo`.
- **One feature group per run.** The rest go to `deferred_groups`.
- **No overlap.** An open feature PR under the same top meta makes the run
  bugs-only (`waiting_on_pr`, rule `rules.no_overlap`). "Under" means a PR
  into `<main>` whose `grind/meta-<X>-…` head names the top meta or one of
  its sub-metas, since a group's feature branch carries its sub-meta's
  number. A feature PR under a different top meta does not block.
- **Abandoned feature PRs.** A stalled or ended run leaves its feature PR
  open (#1819). The router marks a blocking PR *abandoned* when no goal
  landed on it and no run owning it (`clud grind-facts owner <pr>`) has
  made a tool call in 30 minutes; the hook refreshes a run's facts file on
  every lookup, so its age is that run's silence. The question round then
  offers to close it and run the feature stage. A live PR always blocks.
- `grind-run.js` enforces the deferral and the no-overlap rule, not just the
  router.
- **Closing the top meta.** The chosen group's feature PR carries
  `Closes #<sub-meta>` and its children, never the top meta: feature PRs
  merge in any order, so no single PR may close it. Bug children close
  through their own PRs. No grind role closes the top meta.

### Preflight and the single question round

Issue [#1407](https://github.com/zackees/clud/issues/1407); spec
[#1392](https://github.com/zackees/clud/issues/1392) §0 and §2.

- **Order.** Routing → planning pass → preflight → **one** question round
  (at most 2 `AskUserQuestion` calls of at most 4 questions each: repo and
  plan first, run policies second) → prework → run. Nothing is asked
  after prework starts.
- **Dirty repo.** Preflight inspects the checkout. The options are: stash
  as `grind-<run-id>`; commit to a local-only branch `wip/grind-<run-id>`;
  carry the changes into the feature worktree as the feature branch's first
  commit (offered only when a feature stage exists); or abort, which
  creates nothing. The run's own `.clud/grind/` files are excluded from the
  check, the stash and the WIP commit.
- **Recording.** The answers go into the run facts as `preflight` (what was
  stashed or branched), `feature_merge`, `problem_reporting` and `tracks`.
  Finish restores only what `preflight` recorded.
- **Enforcement.** `block_bad_cmd_grind_caps::tool_reason` denies
  `AskUserQuestion` to every `grind-*` subagent. The main-session router is
  bound by its skill text instead, pinned by
  `tests/harness/test_grind_upfront.py`.

The rationale is in
[DD-097](../DESIGN_DECISIONS.md#dd-097-the-grind-run-plans-before-it-asks-one-up-front-question-round-none-after-prework);
the preflight exclusion and carry are in
[DD-101](../DESIGN_DECISIONS.md#dd-101-grinds-preflight-never-touches-its-own-files-and-carry-becomes-the-feature-branchs-first-commit).

### Problem reporting

Issue [#1411](https://github.com/zackees/clud/issues/1411). The
`problem_reporting` answer is recorded as described in
[Preflight and the single question round](#preflight-and-the-single-question-round).

- **Returned, not filed.** Every role may return
  `problems: [{kind, summary, evidence, related_issue}]` alongside its
  result. Roles never file anything themselves.
- **Collected once.** `grind-run` collects the problems from every role and
  dedupes them by `kind` + `summary` + `related_issue`.
- **Only the router files.** It follows `problem_reporting`:
  - `issue`: one issue per problem, body carrying `Refs #<meta>` and the
    marker (no label)
    `<!-- grind:followup meta=<T> stage=<stage> feature-pr=<N> -->`. It is
    never attached as a sub-issue of the meta.
  - `comment`: one comment per problem on its `related_issue`, or on the
    meta issue when there is none.
- **Never blocking.** Follow-ups do not hold up closing the meta issue and
  are not counted by the no-overlap check.
- **Later pickup.** Intake and cron treat a follow-up (its body carries the
  `grind:followup` marker) from the bug stage as eligible at once; one from
  a feature stage becomes eligible only after that stage's feature PR has
  merged.
- **Filing failures.** A failed filing does not stop the run; the router
  lists each unfiled problem inline in its final report.

The rationale is in
[DD-098](../DESIGN_DECISIONS.md#dd-098-grind-follow-up-issues-are-never-sub-issues-of-the-meta).

### Prework and the plan comment

Issue [#1408](https://github.com/zackees/clud/issues/1408); spec
[#1392](https://github.com/zackees/clud/issues/1392) §0.

- **Role.** `grind-prework` is the first agent of the real run, after the
  question round and before any planner or worker. It does not plan; it
  records the plan the router assembled.
- **Local copy.** The router writes the full `grind-plan/v1` object to
  `.clud/grind/plan.json` and passes it to `grind-run` as `args.plan`, with
  the meta issue as `args.meta` (the top meta issue in a meta of metas).
  The run facts also carry `meta`, so the hook knows which issue prework may
  comment on. Finish deletes `plan.json`.
- **Public comment.** Prework posts the plan once on the meta issue, in a
  `~~~json` fenced block under `<!-- grind:v1 plan run=<run-id> -->`, piping
  the body as one single-quoted word:
  `printf '%s' '<body>' | gh issue comment <meta> --body-file -`. clud's
  command hook reads a backtick, `$(`, `<(` or `>(` as a substitution even
  inside quotes, so the workflow keeps backticks, single quotes, `$`, `<`
  and `>` out of the JSON (`\u` escapes). The
  comment is public on public repos, so local-only fields are dropped:
  `preflight` becomes `"handled"`, and the starting branch, stash name, WIP
  branch and local paths never appear. The local details stay in
  `plan.json`.
- **Never edited.** Nobody edits the plan comment, so no decision can change
  mid-run. Every later agent is told its URL and reads it before starting.
- **Size.** GitHub caps a comment at 65,536 characters. Per-child entries stay
  one line; above about 60,000 characters the plan is split into numbered
  comments marked `part=k/N`, linked from the first. A part that would
  still reach the limit stops the run before prework posts anything.
- **Failure stops the run.** If prework cannot post, the workflow returns
  `stopped: 'prework'` before any other agent starts; the router reports
  that the plan was not posted and no work was done.
- **Status comment.** Results live in a separate comment,
  `<!-- grind:v1 status run=<run-id> -->`, one line per child. The router,
  not prework, posts it when the run starts and edits it
  (`gh api -X PATCH repos/<o>/<r>/issues/comments/<id>`) as results arrive
  and at Finish.
- **Caps.** Read-only git and `gh`, plus `gh issue comment` on the
  run facts' meta issue only, fed by `printf '%s' '<body>'`. No Write/Edit,
  builds or worktrees.

Why the body is shell-inert and piped from `printf`:
[DD-102](../DESIGN_DECISIONS.md#dd-102-the-grind-plan-comment-body-is-shell-inert-and-piped-from-printf).

### Bug stage, then feature stage

Issue [#1409](https://github.com/zackees/clud/issues/1409). The stage fields
are defined by the plan; see
[Prework and the plan comment](#prework-and-the-plan-comment).

- **One call per stage.** The feature branch must be cut from `<main>`
  *after* the bug stage has merged, and only the router may create it, so
  the router starts `grind-run` twice. The bug-stage call gets `plan`, the
  bug children and `base: <main>`; prework posts the plan there, and the
  result carries `plan_url`. An all-feature plan still makes this call, with
  no goals, so the plan is recorded before any branch exists. The router
  then sets up the feature branch and starts the feature-stage call with the
  feature children, `base: <feature branch>`, `plan_url` (prework is
  skipped), `feature`, `feature_merge`, `problem_reporting` and
  `stuck_bugs`.
- **Order.** Given goals from both stages in one call, `grind-run` still
  runs every bug-stage goal first and each feature stage after it, against
  that stage's `stage.base`.
- **Base.** A goal's base is its plan stage's `base`; `args.base` (default
  `<main>`) covers goals in no stage.
- **Ids.** Plan stages hold numbers, goals and planner output strings or
  `#N`; the workflow compares them after stripping `#` (`idKey`).
- **Stuck bug.** The rule is `block_dependents_only` (also when the plan
  omits `rules.stuck_bug`). When a bug does not merge, in this call or as
  listed in `stuck_bugs`, only the feature children that list it in
  `depends_on_bugs` are blocked, reported as `blocked: bug #N did not land`.
  Every other goal proceeds.
- **Caps.** While the bug stage runs, the run facts have no `feature`, so the
  lander merges bug PRs into `<main>` under the plain caps.

Why two calls:
[DD-100](../DESIGN_DECISIONS.md#dd-100-the-grind-router-starts-grind-run-once-per-stage).

### Feature-branch mode

Issue [#1410](https://github.com/zackees/clud/issues/1410); spec
[#1392](https://github.com/zackees/clud/issues/1392) §6. Used for
feature-stage children of a meta issue; issue safety is in
[#1393](https://github.com/zackees/clud/issues/1393).

- **Setup.** After the bug-stage call returns and before the feature-stage
  call, the router creates `grind/meta-<M>-<run-id>` from the updated
  `origin/<main>` in the worktree
  `.clud/grind/worktrees/feature` and pushes it. `<M>` is the feature meta:
  the meta issue, or the chosen group's sub-meta in a meta of metas. It
  opens the **draft feature PR** into `<main>` (body `Closes #<M>`, plus
  `Closes #<original>` for a converted issue) and passes
  `feature: {branch, worktree, pr}` and `feature_merge` to the workflow and
  the run facts. The user's checkout is not touched after preflight.
- **Bug fixes reach the branch.** The branch is cut after the bug stage
  has merged, so it already contains those fixes. Anything that lands on
  `<main>` later is merged into it by the next feature-stage integrator (see
  *Merge commit, main merged in* below), never rebased.
- **Goals.** Each goal PR's base is the feature branch, so it still gets CI
  and a review trail. The integrator rebases the goal onto the feature
  branch, writes `Refs #N`, and never edits the feature PR. The lander
  merges the goal PR into the feature branch with `--admin --merge` (never
  `--delete-branch`), and only then adds `Closes #N` to the feature PR's
  body (keeping `Closes #<M>` and every other line), so a goal that never
  lands is never closed by the feature merge. Parallel goal worktrees branch
  from the feature branch; sequential goals run inside the feature worktree.
- **Merge policy** (`feature_merge`, asked up front):
  - `auto`: once CI is green the lander marks the PR ready and runs
    `gh pr merge --merge`, never `--admin`, so branch protection and required
    reviews still apply.
  - `later`: at Finish, when every feature goal landed, the router marks
    the PR ready (`gh pr ready`) and leaves it open for the user; if a goal
    did not land it stays a draft. Nobody merges it: the hook refuses the
    lander `gh pr ready` and the merge, and the router the merge.
  - `comment`: the router posts the result on the meta issue and keeps the
    PR a draft.
- **Merge commit, main merged in.** The feature PR lands as a merge commit.
  If `<main>` moves during the run, `<main>` is merged into the feature
  branch, never rebased, so goal SHAs already merged there stay unchanged.
- **Caps.** `block_bad_cmd_grind_caps.rs` refuses `gh pr merge --admin` on
  the feature PR, requires its merge to name `--merge`, lets the lander
  merge it into `<main>` only under `feature_merge: auto`, and refuses
  deleting a `grind/*` branch (including `gh api -X DELETE …/git/refs/heads/grind/*`)
  while its feature PR is open. In feature mode the lander may also
  comment on an issue (the landing marker), edit only the feature PR's
  body, and `gh run rerun`. It never creates, applies or removes a label
  (#1958).
- **Router caps.** The main session has no agent type, so while the session's run
  facts record a `feature`, the hook applies router caps to any caller that is
  not a grind role: exactly one `git worktree add` (the feature worktree),
  no issue close (`gh issue close` or `gh api … -f state=closed`), no merge
  of the feature PR, and no deletion of a `grind/*` branch. These key on the
  caller's own session, so another session's run never caps it; a crashed
  run's facts keep them on for that session until `clud grind-facts clear`
  (the denial says so) or the 72-hour staleness cutoff.
  Finish removes the run facts last, so cleanup stays guarded.

The rationale is in
[DD-095](../DESIGN_DECISIONS.md#dd-095-the-grind-feature-lands-as-a-merge-commit-and-main-is-merged-into-the-feature-branch-rather-than-rebased).

### Never losing issues in feature mode

Issue [#1393](https://github.com/zackees/clud/issues/1393).

- **Single closer.** Goal PRs into the feature branch say `Refs #N`, never a
  closing keyword. Only the feature PR carries `Closes #N` for each landed
  child plus `Closes #<meta>`, so GitHub closes an issue exactly when its fix
  merges into `<main>`. No role runs `gh issue close` on a feature child.
- **Marker, no label.** When a goal merges into the feature branch the lander
  posts one marker comment and adds the issue's `Closes #N` line to the
  feature PR:
  `<!-- grind:v1 feature-pr=#<N> branch=<feature> goal-pr=#<G> run=<run-id> -->`.
  Grind leaves no labels in the target repo (#1958): the feature PR's
  `Closes` lines make pending issues findable and the last marker names the
  feature PR. Intake and triage skip issues an open feature PR closes.
- **Reconcile.** `clud grind reconcile` (`grind_reconcile.rs`) runs at the
  start of every `/grind` run and every `/grind-cron` tick. For each issue a
  `grind/meta-*` PR's body closes whose last marker has no later
  `<!-- grind:reconcile resolved … -->` tag it reads the marker, the feature PR, and (for a closed issue) the
  GraphQL `ClosedEvent.closer`:

  | Feature PR | Issue | Action |
  |---|---|---|
  | merged into `<main>` | open | close citing the feature PR; resolve |
  | merged into `<main>` | closed | resolve |
  | closed unmerged | any | reopen if closed; resolve; comment citing `refs/pull/<N>/head` |
  | open, or merged elsewhere | closed by hand, by an unmerged PR, by a PR into a non-default base, or by a commit not on `<main>` | reopen with a comment naming the closer |
  | open, or merged elsewhere | closed by a PR merged into `<main>` or a commit on `<main>` | none |
  | open and stale (14 days idle, conflicting, or branch gone) | any | report stale; comment on the meta issue once per staleness window (a hidden `<!-- grind:reconcile … -->` tag suppresses repeats) |
  | open, not stale | any | report it (draft, ready, or waiting for review) |

  It also reopens an untracked sub-issue of a feature meta that was closed
  by hand mid-run, and closes the top meta issue of a meta of metas once
  every child is closed (follow-ups never count). Actions are applied in
  order and a pass stops at the first failure; the `resolved` tag is posted
  last, so a failed close leaves the issue tracked for the next pass.

- **Reachable commits.** A commit closer counts as landed only when
  `compare/<main>...<oid>` reports `behind` or `identical`, i.e. the commit is
  reachable from `<main>`.
- **Branches.** `grind/*` branches are not deleted while their feature PR is
  open (see the caps above and the `/clud-git` playbook), so an unmerged
  feature's commits stay reachable.

Reconcile is idempotent: a healthy open issue yields no action. It is covered
by unit tests in `grind_reconcile.rs` and by
`tests/harness/test_grind_reconcile.py`.

**Reconcile is not a launch, so it carries no harness requirement (#1803).**
It parses as `clud grind reconcile` — i.e. `Command::Grind` — but it reads
GitHub through `gh` and exits with its own status: no harness is spawned, no
`/loop` is seeded, and the `/grind` router skill never loads. The Claude
harness requirement in [`grind_launch_error`](../../crates/clud-bin/src/command/builder.rs)
therefore applies only to the interactive launch, and keys its exemption on
the same `args::is_reconcile` predicate `main.rs` dispatches on, so the gate
and the dispatch cannot drift apart. Before #1803 the gate read the *resolved*
harness — which comes from the saved global preference in
`~/.clud/settings.json` — so a user whose saved default named a non-Claude
harness was refused from inside a session that really was Claude. Its
siblings (`grind-facts`, `grind-scripts`, `clud tool run`) were unaffected
because `dispatch_fast_path_command` handles them before any backend is
resolved, which is how the `grind` family came to disagree with itself.

The rationale is in
[DD-096](../DESIGN_DECISIONS.md#dd-096-the-feature-pr-is-the-single-closer-of-grind-issues-and-clud-grind-reconcile-reopens-early-closes).

### Repository lint/test scripts

Issue [#1336](https://github.com/zackees/clud/issues/1336).

- **Detection.** The router carries a `` !`clud grind-scripts` `` line
  (`command/grind_scripts.rs`), so Claude Code renders the detected facts into
  the skill before the model sees it. The candidates are `lint`, `lint.sh`,
  `lint.bat`, `lint.ps1` and the matching `test*`. On Windows the `.bat` /
  `.ps1` form wins, with the extensionless or `.sh` script (via Git Bash) as a
  fallback. Elsewhere only the extensionless or `.sh` form counts, so a lone
  `test.bat` on Linux means nothing was detected. If a script isn't
  executable, it runs through its interpreter: `bash ./lint`,
  `cmd /c lint.bat`, `pwsh -File lint.ps1`.
- **Modes, by reading only.** For each script the output lists the files to
  read: the script and whatever it delegates to (`python -m ci.test` means
  `ci/test.py`; `bash ci/x.sh` means `ci/x.sh`). The router reads them and
  offers at most four user-facing modes, such as `--integration`, each with a
  short description. It ignores flags the script passes to its own tools
  (`uv`, `cargo`, `pip`, `pytest`). It never runs a script to find its modes,
  because a script that ignores `--help` would start the whole suite.
- **One question per run**, asked only when something was detected: lint and
  test, lint and test with each mode found, lint only, test only, or neither
  (use the planner's verify commands). If nothing was detected, there's no
  question.
- **Recording.** The answer goes into the run facts as `scripts`,
  for example `{"lint": "bash ./lint", "test": "bash ./test --integration"}`,
  and reaches the workflow as `args.scripts`. `{}` means neither.
- **Integrator.** Before **every** push, fix rounds included, it runs the
  goal's focused RED → GREEN test, then lint, then test. A failure loops
  inside the integrator (fix, re-run), because nothing has been pushed yet,
  so it doesn't use up a lander fix round. When scripts are chosen, the
  planner supplies only the focused test.
- **Long scripts.** Anything that may run past the Bash tool's 600-second
  limit runs in the background, and the integrator waits for its exit code.
  Never pipe a script through `tail`, which hides its exit status (#1331).
- **Main already red.** If lint or test already fails on `origin/main`, the
  integrator fixes that too. The fix goes in its own commit ahead of the
  goal's commit, in the same PR, titled like
  `fix: pre-existing lint failure on main`.
- **Caps.** The integrator's shell is a denylist, so the scripts run.
  `grind-worker` and `grind-reviewer` are denied them like any other build,
  lint or test command.
- **Checks are never worker tasks (#1397).** Every planned task writes at
  least one file. `grind-run.js::routeCheckTasks` moves a file-less task
  (for example "verify build and tests") into the integrator's verify
  commands, and a plan left with no file-writing tasks ends the goal
  unmerged instead of running an empty review.

The rationale is in
[DD-091](../DESIGN_DECISIONS.md#dd-091-grind-gets-repo-linttest-scripts-from-a-clud-subcommand-asked-once-per-run).

- **Failures are read, not retried (#1425).** After a failed lint or test
  run the integrator reads the failing tests first. It reruns unchanged only
  for a known infrastructure error, at most twice; the same test failing on
  two consecutive runs is real (fix it, or show it also fails on untouched
  `origin/<main>`). It waits on a background run by its exit, not in fixed
  sleep blocks. The caps hook refuses the integrator a shell loop
  (`for`/`while`/`until`) that runs a lint/test script.

### Review gate, parking and dependents (#1424)

- **"Not yet run" never rejects.** The reviewer cannot run anything, so it
  approves on reading and lists the checks it wants under `must_verify`.
  `grind-run.js` appends them to the goal's verify commands, so the first
  integration and every fix round run them. A rejection whose summary only
  says nothing has been run (`NOT_RUN` in the workflow) is overridden; the
  integrator is told so and must still refuse to push a real defect.
- **Parking.** Sequential goals share one checkout. A goal that wrote files
  and ends unmerged with nothing pushed (rejected, blocked, or its
  integration failed) gets one more integrator call, under the build lock,
  with a `PARK goal` prompt: it commits the goal's paths to a local
  `wip/grind-<goal>` branch, never the preflight's pre-run state, and
  returns the checkout to a detached `origin/<base>`. If the checkout is
  still dirty afterwards, every later goal in the run is blocked. Finish
  keeps and reports park branches.
- **Detached HEAD (#1382).** Parking leaves the shared checkout detached on
  `origin/<base>` between goals, so preflight treats a detached start as a
  fault: reachable from `origin/<main>` means `git switch <main>` and
  fast-forward; otherwise the run stops and reports the commits that exist
  only there. Finish returns with `git switch <branch>`, never
  `git checkout origin/<main>`, and lists `grind/*` and `wip/grind-*`
  branches not merged into `origin/<main>`.
- **Dependents.** `depends_on` is known only after planning, so a goal
  whose dependency already settled unmerged stops right after its plan,
  before any worker writes, with `blocked: dependency X was rejected` (or
  `did not land`, `is blocked`). In parallel mode a dependency that is still
  running is awaited before integration, as before.

### Integration order

- Plan, work and review run at most **4** agents at a time.
- **Cross-check platform implementations before host verification (#1430,
  scoped).** Inspect the diff first. Native API/FFI code, OS branches,
  platform adapters/selectors and runtime contracts select the affected
  targets. Shared business logic calling unchanged platform APIs, helper
  extraction and unchanged moved OS code use the routine gate without
  extra target passes or native labels. `git/ci_targets.py` inventories
  available targets, including release-only lanes; it does not select them.
  Check only affected targets through the repository's isolated runner and
  platform-crate commands (a mixed workspace may require a workspace check).
  Preserve declared routine lint/check/test gates and report unsupported
  affected targets explicitly. Avoid duplicating routine cross-target
  Dylint checks with an ad hoc loop. Label selection follows the changed
  platform implementation; release validation still covers every target.
- Exactly **one** integrator runs at a time. It is the only role that
  builds, so builds never overlap and caches stay warm. No `bosn` wrapper is
  needed or allowed.
  **bosn substitute** (#1818). CLAUDE.md's "run through `bosn ci`" rule
  applies to interactive sessions, not grind roles: the grind-integrator's
  caps hook denies `bosn`, so the integrator never runs it. Inside a run it
  runs the run's chosen scripts (`./lint`, `./test`, or the planner's verify
  commands) directly, in the background when they may exceed 600s and never
  piped through `tail`. The pre-push full-PR bosn / local-gate pass is not
  run by the integrator; GitHub CI on the PR is the CI of record.
  Native-platform coverage gaps are reported, never claimed as covered.
- The planner declares `depends_on`. An isolated goal rebases onto
  `origin/<base>` (`<main>` in the bug stage, the feature branch in the
  feature stage). A dependent goal waits until its dependency merges, then
  rebases onto the new `origin/<base>`.
- Landing does not hold the build lock: while one PR's CI runs on GitHub, the
  next goal integrates locally.
- **Dedup preflight (#2467).** The planner first searches open and merged PRs
  for `Closes #N` (and `#N` titles, `*/N-*` branches). A covered goal returns
  `covered_by` and is skipped and reported `covered by #PR`.
- The lander runs `pr_merge_watch` on the PR's checks. On red CI or new review
  it hands the failure back to the integrator, which fixes, re-verifies and
  pushes. That is at most 10 rounds; after that the PR is left open and
  reported. **Red-base breaker:** a failing required job that also fails on
  `<main>`'s latest `push`/`merge_group` run is `base_red`: no fix round, no
  rerun; the goal is reported `base-red` and the run moves on.
- **A goal is done at enqueue** ([DD-195](../DESIGN_DECISIONS.md#dd-195-grind-goals-end-at-enqueue-a-haiku-chaperone-follows-the-queue),
  the [ci.md agent landing contract](ci.md#agent-landing-workers-end-at-enqueue-chaperones-own-the-merge)).
  When checks are green the lander submits a bug-stage PR with
  `clud tool run github/pr_merge.py -- <PR> --repo <o/r> --match-head-commit <sha>`
  and returns `enqueued`; the workflow starts the next goal at once. Nobody
  rebases before landing: the queue's `merge_group` run tests the merged
  tree. A feature-stage goal PR still merges into the feature branch with
  `--admin --merge`, synchronously, since that branch has no queue.
- **Chaperone.** At enqueue the workflow starts, per PR, one guaranteed,
  deadline-bounded `pr_merge_watch.py --until merged --timeout <queueWatchSeconds, 540>`
  call, run by a `grind-chaperone` agent (haiku, `tools: Bash`, no checkout),
  and collects its verdict by exit code (`0` merged, `4` timeout, `13`
  ejected, `14` unmergeable). The chaperone self-serves per DD-192 (rerun
  `--failed` once for an infra failure, re-enqueue after a batch-mate's
  ejection) and returns `merged`, `queued`, `needs_fixup`, `stuck` or
  `base_red` (MERGED / NEEDS-FIXUP / STUCK).
- **Queue-drop sweep.** Between goals (sequential) and at Finish, the
  workflow sweeps its list of enqueued PRs. A NEEDS-FIXUP PR goes back to the
  integrator ahead of new work (`QUEUE FIXUP`: rebase onto `origin/<base>`,
  re-verify, push), then the lander re-enqueues it; at most `queueFixups`
  (2) automatic fixups, then `stuck`. A still-queued PR is re-watched between
  goals. In parallel mode each goal's chain runs its own fixups under the
  integrator lock. At Finish a still-queued PR gets one final bounded wait
  (`chaperone:<goal>#final`) and is then reported `queued`, not a failure.
- **Dependents of an enqueued goal** wait for its chaperone, not for a sweep:
  `landed[goal]` settles `true` on MERGED and `false` once the chaperone
  pauses on a fixup or timeout, so a dependent is blocked rather than
  deadlocked behind a sweep that cannot run while it does.
- `pr_merge_watch` exits `0` green, `1` failed, `2` new review, `3` closed,
  `4` timeout, `5` approval required (fork `action_required`; report, no
  retry), `6` never reported (required check never ran; blocked), `7` stale
  (older than 14 days; re-run once), `8` no checks (no workflows, a
  `[skip ci]` marker, or nothing registered within the grace period; the
  lander merges only when the base branch has no workflows and
  `mergeStateStatus` is `CLEAN`, and otherwise returns `needs_fix`, never
  `merged`, #1429), `9` conflict (back to the integrator),
  `10` GitHub unreachable (never reported as closed), `11` queued too long
  (opt-in `--max-queued`). A `clud tool run` resumable watchdog stop exits
  `124` with `status: in-progress`; the lander watches again and never merges
  on that code. The lander passes `--timeout 540`, below its
  600 s tool cap, so the watch always exits on its own; a watch never cancels
  runs on a head SHA it did not start on (#1418). Green never waits for
  CodeRabbit by default: nothing can reproduce it under the local
  bosn -> act gate, and the fleet suppresses it with `.coderabbit.yaml`
  (`reviews.auto_review.enabled: false`). A suppressed repo or a head with no
  `CodeRabbit` status goes green at once with `coderabbit=suppressed` or
  `coderabbit=absent`; there is no grace period. Waiting for a `pending`
  review is an explicit opt-in (`--coderabbit-wait N`, bounded by
  `--timeout`; it always ends `0`, #1332). It judges each check by the newest run of
  its workflow, so a superseded cancelled run is not a failure; the full rule
  lives in the watcher's docstring
  (`crates/clud-bin/assets/tools/github/pr_merge_watch.py`).

### Role caps

| Role | Tools (`tools:` frontmatter) | Shell (hook) |
|---|---|---|
| `grind-orchestrator` | Read, Grep, Glob, Bash | AST-validated read-only inspection; no fetch, edits, builds or checkouts |
| `grind-gate` | Bash | exactly one assigned read-only Bash check, bound to native agent identity and approved run facts; readiness requires its successful native outcome |
| `grind-planner` | Read, Grep, Glob, Bash, WebSearch, WebFetch, Skill | read-only git and `gh`; `git worktree add` in parallel mode only, never in the plan-only phase (the hook also refuses it Write/Edit); `clud grind-facts task` to record each task's files |
| `grind-prework` | Bash, Read, Grep, Glob | read-only git and `gh`; `gh issue comment` on the run facts' meta issue only, fed by `printf '%s' '<body>'`; no edits, worktrees or builds |
| `grind-worker` | Read, Edit, Write, Grep, Glob, Bash, WebSearch, WebFetch, Skill | read-only `gh`; `safe-rm` on literal paths inside its task's directories ([rm-tools.md](rm-tools.md#location-policy)) |
| `grind-reviewer` | same as worker | same as worker |
| `grind-integrator` | Read, Edit, Write, Grep, Glob, Bash, WebSearch, WebFetch, Skill | anything except `bosn`, direct `docker`/`podman`, `git worktree add`, `act` when CI is off, and a shell loop around lint/test; `safe-rm` only inside its checkout |
| `grind-lander` | Read, Grep, Glob, Bash, Skill | `gh pr view\|checks\|diff\|list\|merge`, `gh pr ready` on the feature PR under `feature_merge: auto` only, `gh run view\|list\|rerun`, read-only git, plain `git push`, `pr_merge_watch`, `pr_merge.py` (enqueue) |
| `grind-chaperone` | Bash (haiku) | read-only `gh` (`pr\|run view\|list\|checks`), `gh run rerun <id> --failed`, `pr_merge_watch`, `pr_merge.py` (re-enqueue), `ci_failure_report.py`; no git, checkout, build, push or direct merge |

Claude Code enforces the tool lists. Shell commands are enforced by clud's
native PreToolUse hook (`clud-block-bad-cmd`): the harness puts the calling
subagent's `agent_type` in the payload, and
`block_bad_cmd_grind_caps.rs` applies that role's policy. Commands it cannot
decompose (command substitution, subshells) are refused for capped roles, as
are `find -exec`/`-delete`, `tail -f`, and git global options other than
`-C`. The lander and integrator wait on CI only through `pr_merge_watch`:
`gh run watch`, `gh run view --watch` and `gh pr checks --watch` are refused
with a message naming it (#1339). Every capped role searches with the Grep
and Glob tools: a shell `grep -r` and a `find` without `-maxdepth` are
refused, and `git grep` is a read-only git command (#1339). Every role may
run the read-only `git/state.py` and `code/outline.py` tools; workers and
reviewers may run `code/patch.py`, which itself refuses files outside the
recorded task list. Every role may search and preview with
`code/ast_grep.py`; only the integrator applies a rewrite (`-U`, `-i`, `new`). The integrator's bans also look inside wrappers such as `bash -c`.
A legacy planner dependency may name only goals listed before it. An approved
run graph is validated for cycles and ordered before scheduling; the planner
cannot drop its merge dependencies. `CLUD_ALLOW_ALL_CMDS=1` turns the whole command hook off, these caps
included.
Other agents and the primary session are unaffected. The tests next to it
own the exact allowlists. On top of the table, every `grind-*` role is
denied `AskUserQuestion`, `gh issue create` and closing an issue.

## Context pack (#1339, #1344)

The planner returns `context` with its plan: the goal issue's body and
comments verbatim, and the code excerpts the goal needs (`{path, start, end,
why, text}`). `grind-run.js` renders it once per goal (`packOf`) and puts it
**first** in every worker's and the reviewer's prompt, ending at
`--- end of context pack ---`; nothing per-agent (task id, file list, round)
may precede that line. Workers then neither re-fetch the issue nor re-read
the excerpted ranges. Claude Code puts system reminders ahead of each
agent's prompt, including a **git status snapshot taken when that agent
starts**, and indents every line of a workflow's computed task text. So
workers' requests are byte-identical up to the pack's end only while they
start against the same checkout state: a worker started after another has
written files sees `?? <file>` where the first saw `(clean)`. Parallel
workers therefore start together (a staggered start, tried first, made the
first worker's prefix diverge from the rest), but startup order is still not
guaranteed: a worker that starts after a sibling's first write misses the
shared prefix at that status line; sequential workers each see
the previous one's changes and do not share. The reviewer, a different agent
type, reads the same pack without sharing the workers' cache.
`tests/harness/test_grind_context_pack.py` asserts the prefix on the real
Claude Code with that status block masked. The real-model cost measurement #1344 calls for stays manual.

## Tests

The workflow, the agent types and the caps run end to end on the real Claude
Code in `tests/harness/test_grind.py`
([testing-tiers.md](testing-tiers.md)). The mock backend plays each role, and
the suite checks the ordering, concurrency, fix-round and cap behavior above.
The router's questions are model behavior, which a scripted model can't
exercise, so the suite starts `grind-run` directly and pins the router text
separately.

The per-feature suites in `tests/harness/`, each named for the section above
it covers:

- `grind_launch.rs`'s
  `grind_launches_the_workflow_session_on_every_non_anthropic_claude_route`
  (Rust) — third-party routes launch the Workflow session.
- `test_grind_routing.py` — input routing and the `/do` refusal.
- `test_grind_plan_only.py` — the plan-only pass and threshold.
- `test_grind_meta_of_metas.py`, `test_grind_overlap.py` — regroup and no
  overlap.
- `test_grind_upfront.py` — preflight and the single question round.
- `test_grind_prework.py` — the plan comment.
- `test_grind_stages.py`, `test_grind_tracks.py`, `test_grind_feature.py` — stages and
  feature-branch mode.
- `test_grind_problems.py` — problem reporting.
- `test_grind_reconcile.py` — reconcile.
- `test_grind_review_gate.py` — the review gate, parking, dependents of a
  rejected goal, and the integrator's retry rules.
- `test_grind_scripts.py` — the repository lint/test scripts.
- `test_grind_facts.py` — run facts: a workflow agent's payload carries the
  parent session id, `clud grind-facts path` names the file the hook reads,
  and each session is capped by its own run's facts.
- `test_grind_e2e.py` — the three user scenarios posted on
  [#1392](https://github.com/zackees/clud/issues/1392), end to end, each
  from routing to Finish:
  1. a meta issue of unrelated bugs: every PR goes into `<main>` with
     `Closes #N`, and one problem becomes a follow-up issue that is
     not a sub-issue;
  2. a prompt with prerequisite bugs, a feature and a dirty checkout, under
     `auto`: the checkout is stashed, the bugs land first, `<main>` is
     merged into the feature branch, and the one feature merge commit closes
     the meta issue and every feature part;
  3. a regrouped epic under `later`: the user's sub-metas are rewritten in
     place, one group runs while the other is deferred, the feature PR stays
     open, the next run is bugs-only and names it, and the user's merge
     closes the group.

GitHub itself is faked (`tests/harness/fake_gh.py`), so one step stays
manual: a smoke run against a scratch GitHub repository, checking that the
closing-keyword rule closes children only when the feature PR merges into
`<main>`, that `replace_parent` moves children during a regroup, and that
a large feature PR's body stays within GitHub's closing-keyword count.

## Boundary with `clud loop`

`clud loop` remains a separate command with its own external runner,
DONE/BLOCKED contract, iteration budget, artifacts, and optional repeat
scheduler; see [the loop subsystem](loop-subsystem.md). `grind`'s cron mode
uses the harness's `/loop`, never clud's.

## Implementation review checklist

- One backend harness session per `clud grind`, launched per DD-086 (PTY
  from a console), with a prompt starting `/grind`.
- No loop markers, repeat schedule, external iteration count, or stream-json
  setting in the plan.
- Unsupported harnesses fail before a backend process is spawned.
- A new `grind-*` role needs its agent file, a `claude_files.rs` entry, and a
  policy arm plus tests in `block_bad_cmd_grind_caps.rs`.

See [DD-087](../DESIGN_DECISIONS.md#dd-087-grind-is-a-skill-dag-with-capped-agent-roles)
for the rationale; it supersedes [DD-068](../DESIGN_DECISIONS.md#dd-068-grind-delegates-looping-to-the-interactive-harness)'s
direct `/loop` prompt.

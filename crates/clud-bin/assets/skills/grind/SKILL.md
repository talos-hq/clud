---
name: grind
description: "Work through a list of goals (a meta issue, issue list, free-form goal, or the repo's issues page) to merged PRs: plan, write, review, integrate and land, in parallel worktrees, sequentially in the local checkout, or one issue per /loop tick. Router for the grind-* skills and the bundled grind workflow."
triggers:
  - When the user runs /grind or clud grind
disable-model-invocation: true
allowed-tools: Bash(clud grind-scripts:*), Bash(clud grind-facts:*), Bash(clud claim acquire:*), Bash(clud claim release-own:*)
---
<!-- managed-by: clud -->

!`clud grind-scripts`

The line above lists the repository's `./lint` and `./test` scripts and the
files they delegate to. If it still shows a command instead of that report
(the harness did not run it, or `clud` is missing), run `clud grind-scripts`
yourself from the repository root. If that fails too, treat the repository as
having no scripts and skip the scripts question.

# /grind

Before intake or any checkout mutation, run `clud claim acquire` in the
current checkout. If the daemon is unavailable or another session holds the
claim, stop and offer a new worktree or a retry after the claim releases.

Router for the grind DAG. It gathers every answer up front, then hands the
deterministic part to the bundled `grind-run` workflow.

```
/grind ─ intake ─ orchestrator + graph approval ─ classify (plan-only) ─ preflight ─ configuration ─ prework ─┬─ parallel   ─┐
                                                                                          ├─ sequential ─┼─ workflow grind-run: plan → work → review → integrate → land ─ finish
                                                                                          └─ cron ─ /grind-cron ─ /loop: sequential run, one issue per tick
```

From the moment prework starts (the run's `grind-run` workflow, after the
question round), nobody asks: no agent and not the main session calls
AskUserQuestion, and clud's hook denies it for
every `grind-*` subagent. Anything unexpected follows a rule recorded in
advance in the run facts (section 3).

Every code change keeps a RED -> GREEN focused regression: the integrator
first shows the failure or reproduction, then makes it pass before the
repository's broader gates.

## Platform coverage selection

Native PR coverage follows the changed platform implementation: native API/FFI
handling, OS branches, adapters/selectors or native runtime contracts. Shared
business logic and callers of unchanged platform code use the routine gate.
Name the implementation and select only affected targets; target inventories,
helper extraction and local coverage gaps do not justify `ci-full` or
`ci-windows`. Preserve routine checks and full release validation.

## 0. Harness

`/grind` needs Claude Code's Workflow tool and the bundled `grind-*` agent
types. If either is missing (a Codex or other native harness), stop and say:
"/grind requires the Claude harness; relaunch with `--harness claude`." Do
not emulate the workflow by hand.

## 0b. Reconcile

Grind is client-side: it never creates a label and never applies,
removes or searches by a `grind:*` label in the target repo (#1958). Its
GitHub state is the feature PR's `Closes` lines and hidden HTML markers
(`grind:v1`, `grind:followup`, `grind:reconcile`) in bodies and comments.

At the start of every `/grind`, before intake, run `clud grind reconcile`
from the repository root and report what it prints: each issue it reopened,
closed or resolved, every open feature PR with its state (draft, ready,
waiting for review) and the issues waiting on it, and each stale feature PR.
It reads only GitHub (feature PR `Closes` lines and `grind:v1` markers),
never the run facts. A failure is reported, not fatal. Optionally, mention once that a GitHub Action could run the same
reconcile on a schedule; setting one up is out of scope for this run.

## 1. Intake

Read `../grind-intake/SKILL.md`, next to this skill's base directory, and follow it with the user's argument (the leaf skills are hidden from the model, so they are read rather than invoked). It returns the goal list
(`[{id, title, brief}]`) in order, plus the `meta` issue, the repository root,
the resolved `github_repo` (`OWNER/REPO`) and its default branch (`main` or
`master`). Use that slug as `<github-repo>` in every repository-scoped `gh`
command; API endpoints use its owner/name directly. Keep the checkout path in
`repo` and the forge slug in `github_repo`. Preserve the same slug in
`plan.repo` and every workflow call. If either identity is missing or they
disagree, stop before delegating; never fall back to a fork's gh default.

Intake always resolves the input to a meta issue: it creates or attaches one
for a prompt or issue list, or converts a multi-part issue after asking. That
conversion question (or, with no argument, the pick of which open issues to
take) is intake's only question and comes before section 1b.
Intake may instead end the run with the Abort refusal (a multi-part issue
the user declined to convert; "run `/do N`"), "Nothing to do", or a `gh`
error; then stop there: skip every later section (Finish
included), write no run facts, and create nothing.

**Goals in several repositories (#1422).** When the goals resolve to issues
in more than one repository, group them by repository, in order. Each group
gets its own checkout (a sister checkout per `/clud-extern-repos`, created
only with the consent rules that skill states), `github_repo` and default
branch. Ask the Scripts and CI questions once per repository; Mode, Models
and Problem reporting stay shared. Record one `repos` entry per group
(section 3), and in section 4 start one `grind-run` per repository **in the
same message**, so they run concurrently, each with that repository's
`repo`, `github_repo`, `main`, `scripts`, `ci` and goals. Never put two
repositories' goals in one `grind-run`: it holds one `repo`, one `main` and
one integrator, and its caps come from one repository's facts. A goal that
depends on another repository's goal waits for that workflow's result before
its own workflow starts; say so in the plan. Finish reports and cleans up
each repository separately.

## 1a. Run orchestration before goal planning

After intake resolves the goals, call the native Agent tool exactly once
with `subagent_type: "grind-orchestrator"`. Supply the original user request,
every approved goal (including its full issue URL and brief), the session
repository identity and absolute checkout root, and the involved repository
roots already discovered by intake. Ask what blocks what across the entire
run. Do not start a goal planner, classification Workflow, worker or
checkout mutation while the advisor is running.

Keep its returned `{session_repo, session_root, goals, proposed_goals, risks}` as
`orchestration`, separate from the later `grind-plan/v1` classification.
Require every original goal exactly once with its repository, actual
default branch, checkout choice and blockers. Resolve missing or ambiguous
repository evidence before proceeding. Do not replace a sister repository's
default branch with the session repository's branch.
Retain the approved absolute `session_root`; every workflow call must use that
same `repo` path and the approved session repository's default branch. If
classification introduces an edge absent from the graph, resolve it and obtain
approval of the revised graph before execution; do not start contradictory
dependency waits.

Show the run graph with a lane per repository, each dependency's condition
and check, checkout risks, and any proposed goals. Ask one approval question:
**Run it / Run it with the proposed goals / Cancel**. Omit the second option
when there are no proposals. Cancellation ends the run before classification
or writes. Approval of the existing goals does not approve proposed work,
worktree creation, dirty-checkout changes or additional permissions.

Only explicit approval of proposals adds those goals; retain their rationale
and blockers and include them in the approved graph. Keep upstream PR URLs,
tags and versions as evidence to be forwarded when they exist; never guess
them. Do not invoke the advisor again for each goal or stage. Subsequent
configuration questions use this graph to ask about CI, scripts and checkout
consent for each repository. The per-goal planner still splits one goal into
tasks; it cannot remove the approved run dependencies.

## 1b. Plan before asking (classification)

**The run facts.** clud's command hook reads this run's facts from one file
per session, outside the repository: run `clud grind-facts path` once and
use the absolute path it prints (`~/.clud/tmp/grind/<session>.json`) as
`<facts>` everywhere below. Write and rewrite it with the Write tool; never
put run facts in the working tree. Another `/grind` run, in this repo or any
other, has its own file, so runs never share or clobber facts.

After intake resolves the meta issue `T`, write `<facts>` as:

```json
{"phase": "plan"}
```

clud's command hook then denies the planner Write/Edit, `git worktree add`
and `git push`. Start the Workflow named `grind-run` with args
`{repo, github_repo, main, meta, goals, orchestration, planOnly: true}` and relay its output. The
Workflow tool returns at once and its result arrives later as a task
notification: end your turn and do nothing else until it arrives.

The classification is automatic: print the returned `lines`, one per child,
`#N bug → main` or `#N feature → grind/meta-<T>-<group>`, and never ask
about it.

The workflow also returns `path` and a one-line `message`:

- `simple` (the default) keeps the meta issue exactly as written: no new
  issues, no moved parents, no rewritten bodies. Print the `message`,
  `keeping #T as is: <reason>`; there is no regroup question.
- `regroup` happens only when all of these hold: at least 2 independent
  feature groups of 3+ children each, 8+ children in total, and a confident
  classification with every child placed. Only then is the regroup plan
  shown in the question round.

**No overlap.** Before the question round, run
`gh pr list --repo <github-repo> --state open --base <main> --search 'head:grind/meta-' --json number,headRefName,isDraft`
(`T` is the top meta issue). A PR is a feature PR *under `T`* when its head
is `grind/meta-<X>-…` and `X` is `T` or one of `T`'s sub-issues
(`gh api repos/<o>/<r>/issues/<T>/sub_issues`): in a meta of metas the
feature branch carries its sub-meta's number (section 2b).

**Live or abandoned.** A run that stalls or whose session ends leaves its
feature PR open, and nothing else ever closes it (#1819). So for each
feature PR under `T`, read `gh pr view --repo <github-repo> <n> --json commits,updatedAt,body`
and `clud grind-facts owner <n>`, which lists every run whose facts record
that PR with `idle_secs`, the time since that run's last tool call (the hook
refreshes the facts file on every lookup). The PR is *abandoned* when no
goal has landed on it (every goals-table row is still `pending` and it has
no commit beyond the opening one) and either no run owns it or every owner
has been idle for 30 minutes or more. Anything else is *live*. Report which,
with the evidence (`feature PR #<n> (run <id>): no goal landed, owner idle
<m> min`).

A live PR, or an abandoned one the user keeps in the question round
(section 2, Blocking feature PR), makes the plan bugs-only:
move every feature group into `deferred_groups`
(no feature stage, no branch), set `rules.no_overlap: "bugs_only"` and
`"waiting_on_pr": <n>` in the plan, skip the regroup, feature-pick and
feature-merge questions, and report
`feature PR #<n> for #T is still open; this run is bugs-only`. `grind-run`
reports the feature children as `deferred` and never works them. A feature
PR under a different top meta never blocks; the scope is per top meta. Once
that PR is merged (or closed), the next run may pick a feature again.

Keep the per-child tracks (`bug` or `feature`) for section 3. Section 3 then
overwrites `<facts>` without `phase`, lifting the plan-only caps.

## 1c. Repo-state preflight

Pick the run id now (4 lowercase hex characters); every later
`<run-id>` is this one. Then, read-only, from the repository root:

- `git status --porcelain -uall -- . ':(exclude).clud/grind'` (`-uall`
  lists untracked files one by one, so the exclusion applies to each)
- `git rev-parse --abbrev-ref HEAD` (record it as the starting branch)
- `git stash list`
- `git fetch origin`, then
  `git rev-list --left-right --count origin/<main>...HEAD` (behind, ahead).

**Detached HEAD.** If the starting branch is `HEAD`, the checkout is
detached (a park, or an earlier failed run, left it there). Never build on
it. If `git merge-base --is-ancestor HEAD origin/<main>` succeeds, every
commit is already on `origin/<main>`: `git switch <main>`, then
`git pull --ff-only origin <main>`, and record `<main>` as the starting
branch. Otherwise stop, create nothing, and report the commits that exist
only here (`git log --oneline origin/<main>..HEAD`) so the user can branch
them. Return to a branch with `git switch <main>`, never
`git checkout origin/<main>`, which detaches again.

`.clud/grind/` holds the run's own files (`plan.json` and the
feature worktree), never the user's changes: every preflight command here
excludes it with that pathspec, so it is never reported, stashed or
committed.

Repo state is asked about here and nowhere else. If the tree is dirty, the
question round includes the dirty-repo question (section 2). Apply the chosen
action right after the round, before prework:

- **Stash**: `git stash push -u -m grind-<run-id> -- . ':(exclude).clud/grind'`.
- **WIP branch**: `git switch -c wip/grind-<run-id>`, `git add -A -- .
  ':(exclude).clud/grind'`, `git commit -m "grind: WIP before run <run-id>"`,
  then `git switch <starting branch>`. Never push it.
- **Carry into the grind worktree**: offered only when the plan has a
  feature stage. Stash now, as
  `git stash push -u -m grind-<run-id>-carry -- . ':(exclude).clud/grind'`,
  so the user's checkout is clean; section 4b applies it in the feature
  worktree.
- **Abort**: stop. Create nothing on GitHub or in git, remove the
  plan-phase run facts with `clud grind-facts clear`, and write no other
  file.

A clean tree records `preflight.action: "none"`.

**Feature-base CI triggers (#1893).** When the plan has a feature stage,
check that a goal PR into the feature branch triggers CI at all:
`"$CLUD_EXE" tool run github/pr_merge_watch.py -- --repo <github-repo> --triggers-for-base grind/meta-<M>-<run-id>`.
It prints `{pr_workflows, dispatchable}`. An empty `pr_workflows` (every
`pull_request` workflow filters that base out) goes in the plan comment and
adds one question to the round, before any work: dispatch CI per goal head
(`gh workflow run <wf> --ref <goal-branch>` for a `dispatchable` workflow,
watched with `github/run_watch.py`), widen the workflow's `branches:` filter
as a bug-stage goal, or validate locally only. Record the answer as
`preflight.ci_fallback`; the lander applies it to `no_trigger_for_base`.

## 2. One question round

At most 2 AskUserQuestion calls in total, and nothing is asked twice.
AskUserQuestion takes at most 4 questions per call and 2-4 options per
question (the user can always type another answer), so the round is fixed:

- **Call 1, the repo and the plan:** Dirty repo, Regroup or Blocking feature PR, Mode, Models.
- **Call 2, the run's policies:** Local CI, Scripts, Feature merge policy,
  Problem reporting.

Skip an item whose condition does not hold. Problem reporting is always
asked, so a run that goes ahead makes exactly 2 calls; answering Abort in
call 1 stops before call 2. This is the only round: no question is asked
after it, by the main session or anyone else.

- **Dirty repo** (only if 1c found changes): show the file list; options
  Stash it, Commit to a WIP branch, Carry into the grind worktree (only when
  the plan has a feature stage), Abort.
- **Blocking feature PR** (only when 1b found an *abandoned* feature PR
  under `T`; it replaces the Regroup item, which a bugs-only plan skips):
  show the 1b evidence; options **Close #<n> and run the feature stage**
  (Recommended) and **Keep #<n>; bugs-only this run**. On Close, after the
  round and before prework: `gh pr close --repo <github-repo> <n> --delete-branch --comment
  "Closing: abandoned by grind run <id> (no goal landed); run <run-id>
  takes over."`; if `.clud/grind/worktrees/feature` still holds that
  branch, check it has no unpushed work (`/clud-git`) and remove it with
  `git worktree remove` and `git branch -D`; then keep the feature stage
  (no `waiting_on_pr`). A live PR is never offered for closing.
- **Regroup** (only when 1b returned `regroup`): ONE single-select question
  that both confirms the regroup plan
  (`bugs: #b1 #b2 · F1 "<name>": #c1 #c2 · F2 "<name>": #c3 #c4`) and picks
  exactly ONE feature group for this run: `Regroup; run <group> first` per
  group, listed in the planner's dependency order (a group sits where its
  first child sits in `order`), the first labelled "(Recommended)" as the
  default (at most 3), plus "Keep as is (simple schedule)", which declines
  the regroup: the `simple` path runs and no issue is touched. Every other
  group goes into the plan's `deferred_groups` as
  `{group, sub_meta, children}` with no branch assigned.
- **Mode.** (i) *Parallel*: each goal gets a git worktree; workers only read
  and write files; one integrator at a time rebases, lints, builds and tests.
  Uses GitHub's server-side concurrency. (ii) *Sequential*: one goal at a
  time from `origin/<main>` in the local checkout, no worktrees or sister
  clones; best for C++/Rust or other heavy repos where a cold worktree build
  is expensive. (iii) *Cron*: one issue per `/loop` tick, each tick a
  sequential run.
  For a Rust repo, offer *Sequential* first as "(Recommended)" and state
  parallel mode's cost in its option text: every worktree starts a cold
  Cargo target, about 46–72 s and 0.9 GB per worktree for a `check` alone
  (#1685, #1971).
  In parallel mode, keep Cargo targets private while integrators can build or
  test different branches concurrently. An operator may opt in to a shared
  `CARGO_TARGET_DIR` only when builds and tests are serialized, following
  `/clud-git`'s Rust target directory guidance (#1685).
- **Models**, one question for the planner, worker, reviewer and integrator
  (the lander shares the integrator's, prework the planner's): first "the
  session's model for every role", labelled "(Recommended)", then up to 3
  mixes of the other tiers the harness accepts (for example a stronger tier
  for planner and reviewer only). A typed answer may name a model per role.
  The session model is whatever this session is running on (for
  `clud --deepseek` or another provider, the model that route resolved);
  never assume a fixed model name. On a direct gateway route
  (`--openrouter`, `--deepseek`, `--kimi`) the launch is pinned (DD-077):
  the tier aliases `opus`, `sonnet` and `haiku` resolve to the pinned id, so
  offer the mixes as aliases. A typed full wire id is sent to the gateway
  as is and is billed outside the pin, which clud cannot refuse on that
  route; record such an answer only after saying so in the question.
- **Local CI**, asked only when both hold:
  - `docker info` succeeds. Otherwise print
    "Docker/github actions disabled due to no docker running" and set CI off.
  - `.github/workflows/ci.yml` exists. Otherwise print
    "No .github/workflows/ci.yml; local GitHub Actions run skipped" and set CI off.

  The question: run the relevant `ci.yml` job locally under `act` before
  each push? This choice is local-only. Remote CI always runs: grind only
  adds testing, so a user's "skip CI" means skip local validation, never a
  `[skip ci]` marker, a removed CI-lane label or a disabled workflow (the
  hook refuses those for every grind role, #1429).
- **Scripts**, asked once per run and only when the `clud grind-scripts`
  report above found a `./lint` or `./test`: "Found `./lint` and `./test`.
  Run them before each push?" (name only what was found). Options: lint and
  test together, each alone when both exist, and up to four test modes (for
  example `./test --integration`) discovered by *reading*, never executing,
  the delegation files the report lists; their help text and comments give
  each option's description. Always include a "Neither" option, and keep the
  total at 4 options (the most likely ones); the user can type any other.
- **Feature merge policy** (asked once, only when the plan has feature
  children): `auto` merges the feature PR once it is green, respecting
  branch protection; `decide later` leaves the draft feature PR open for the
  user; `comment only` keeps the PR draft and posts one result comment on
  the meta issue. Never `--admin`. Record the answer as `feature_merge`
  (`auto`, `later` or `comment`) in `<facts>` and the plan.
- **Problem reporting** (always): a new issue per problem (Recommended), or
  one comment per problem on the relevant issue.

## 2b. Regroup (meta of metas)

Only when 1b returned `regroup` and the user accepted it, after the
question round and before prework:

- One sub-meta issue per feature group; bug children stay direct children of
  the top meta `T`. With exactly one feature group, create no sub-meta: `T`
  itself is the feature meta.
- The existing sub-metas are the sub-issues of `T` that have sub-issues of
  their own. One is grind-made when its body carries the marker
  `<!-- grind:v1 -->`; a user-made one that
  an earlier run rewrote carries the marker, so it counts as grind-made too.
  Grind-made sub-metas are kept as groups as they are: no rewrite, and their
  children are never re-parented.
- Reuse user-made (unmarked) sub-metas first, in place: rewrite each with
  `gh issue edit --repo <github-repo> <n> --title ... --body-file <f>`, where the new body starts
  with `<!-- grind:v1 -->` and ends with
  `<details><summary>Previous content</summary>\n\n<old body>\n\n</details>`.
  Only groups beyond the reused count get new issues: `gh issue create --repo <github-repo>` with
  the marker body (no label), then
  `gh api -X POST repos/<o>/<r>/issues/<T>/sub_issues -F sub_issue_id=<id>`.
  `sub_issue_id` is always the issue's REST `id`
  (`gh api repos/<o>/<r>/issues/<n> --jq .id`), never its number.
- A leftover user-made sub-meta (more sub-metas than groups) is rewritten to
  say `no children after regrouping`, its old body kept in the same
  `<details>` block.
- Move each feature child with
  `gh api -X POST repos/<o>/<r>/issues/<sub>/sub_issues -F sub_issue_id=<child id> -F replace_parent=true`.
- **Nesting guard.** Before creating or moving anything, check that every
  move keeps the depth at or below 8 levels and each parent at or below 100
  sub-issues. If any move would break either limit, or a sub-issue call
  still fails with GitHub's 422, stop cleanly before prework: create or move
  nothing further, report `regroup failed: <reason>` with the changes
  already made (from `undo`), and fall back to `simple`.
- **Undo record.** Append every change to `<facts>` under `"undo": [...]`,
  one entry per change, recorded before its command runs:
  `{"op": "create", "issue": n}`,
  `{"op": "reparent", "issue": child, "from": old_parent, "to": new_parent}`,
  `{"op": "rewrite", "issue": n, "title": old_title, "body": old_body}`.

Then set the plan's `structure: "meta_of_metas"`, the chosen feature stage's
`sub_meta` to its sub-meta number (`null` when `T` is the feature meta), and
`deferred_groups` for the other groups. `meta` in `<facts>` stays the top
meta `T`, and section 3 keeps the `undo` array when it rewrites `<facts>`.
In section 4b, `<M>` in the branch name and `<meta>` in the `Closes` line
are then the chosen group's sub-meta (`T` when `sub_meta` is `null`), which
is how 1b's no-overlap check finds that PR on later runs.

## 3. Record the run

Write `<facts>` (the path `clud grind-facts path` printed):

```json
{"mode": "parallel", "ci": false, "scripts": {"lint": "./lint", "test": "./test --integration"},
 "preflight": {"action": "stash", "branch": "main", "stash": "grind-<run-id>"},
 "feature_merge": "later", "problem_reporting": "issue",
 "tracks": {"12": "bug", "13": "feature"}, "meta": 100,
 "undo": [], "waiting_on_pr": null}
```

For goals in several repositories, write
`{"repos": [{"repo": "<abs checkout>", ...the object above for that repo}, ...]}`
instead. The hook picks the entry whose `repo` contains the calling agent's
working directory, and applies the strictest caps when none or two match.
Each entry's `meta` must be an issue in that entry's own repository (its
tracker), never the top meta of another repository: that lane's prework
posts its plan there, and the hook lets prework comment on any entry's
`meta` (#2468).

`undo` is the section 2b change record (empty without a regroup).
Also include `orchestration` with the complete graph the user approved in
section 1a, preserving its session root, repository, goal IDs and blocker
order/checks. Do not copy unapproved proposed goals into that graph.
`waiting_on_pr` is the open feature PR that made the run bugs-only under the
1b no-overlap rule, or `null`.

`scripts` holds the chosen commands; omit a key the user did not pick, and
set `scripts` to `null` (or omit it) when they chose "Neither" or none were
found. `preflight.action` is `stash`, `wip` (with `"wip":
"wip/grind-<run-id>"` instead of `stash`), `carry` (with `"stash":
"grind-<run-id>-carry"`) or `none` (clean tree); `branch` is the
starting branch. `preflight` is what Finish restores, and nothing else. `feature_merge` is `auto`, `later` or `comment`, omitted or
`null` when the run is bugs-only. Section 4b adds
`"feature": {"branch", "worktree", "pr"}` when the feature stage starts. `problem_reporting` is `issue` or `comment`.
`tracks` maps each child to its 1b track. `meta` is the meta issue number
(the top meta issue for a meta of metas).

clud's command hook reads it to apply the per-role caps (the planner may add
worktrees only in parallel mode; the integrator may run `act` only when CI is
on; `grind-prework` may comment only on `meta`). Remove it with
`clud grind-facts clear` when the run ends (Finish, step 3).

## 3b. Assemble the plan

The contract (public vs local fields, marker, size split, status comment)
lives in `docs/architecture/grind.md`, "Prework and the plan comment". Build
the `grind-plan/v1` object from intake, the 1b plan, the preflight and the
answers, keeping the real preflight details locally:

```json
{"schema": "grind-plan/v1", "run_id": "1f3a", "meta": 100, "original": 95,
 "repo": "zackees/clud", "main": "main", "mode": "parallel",
 "preflight": {"action": "stash", "branch": "main", "stash": "grind-1f3a"},
 "structure": "simple",
 "stages": [
   {"stage": "bugs", "base": "main", "children": [101, 103]},
   {"stage": "feature", "group": "auth rework", "sub_meta": null,
    "branch": "grind/meta-100-1f3a", "base": "grind/meta-100-1f3a",
    "children": [102, 104, 105], "depends_on_bugs": {"104": [103]}}],
 "deferred_groups": [], "feature_merge": "auto", "problem_reporting": "issue",
 "models": {"planner": "…", "worker": "…", "reviewer": "…", "integrator": "…"},
 "ci": false, "scripts": {"lint": "./lint", "test": "./test"},
 "rules": {"stuck_bug": "block_dependents_only", "no_overlap": "bugs_only"}}
```

Only one feature stage appears in `stages` per run (on the simple path it
holds every feature child); other feature groups wait in `deferred_groups`. Write it to `.clud/grind/plan.json` at the repository root. The workflow's
first agent, `grind-prework`, posts the public copy (preflight reduced to
`"handled"`) under `<!-- grind:v1 plan run=<run-id> -->`, and every later
agent gets that comment's URL. Nobody edits the plan comment.

## 4. Run

- Cron: read `../grind-cron/SKILL.md` and follow it with the goal source and the answers.
- Parallel or sequential: start the Workflow named `grind-run` once per
  stage, because the feature branch is cut only after the bug stage ends.
  1. **Bug stage** (always, even when it has no children): args
     `{repo, github_repo, main, mode, goals, orchestration, ci, scripts, plan, meta, base: <main>,
     problem_reporting, models: {planner, worker, reviewer, integrator}}`,
     where `plan` is the 3b object and `goals` are the bug-stage children
     only (`[]` for an all-feature plan: the call then only posts the plan).
     Its first agent, prework, posts the plan; the result's `plan_url` is
     that comment.
  2. **Feature stage** (only when the plan kept a feature stage): do 4b,
     then start `grind-run` again with the same args, except `goals` are
     the feature stage's children, `base` is the feature branch, plus
     `plan_url` (from step 1, so prework is not repeated), `feature`,
     `feature_merge`, and `stuck_bugs`: the bug children step 1 reported
     as not merged. The workflow blocks each feature child that lists one
     of them in `depends_on_bugs`, and runs the rest.

  Omit a model the user left at the session default, and omit `scripts`
  when none were chosen. Each call's result arrives as a task
  notification (as in 1b): the next step starts only after it arrives,
  and nothing is asked while waiting.
- If the workflow returns `stopped: 'prework'`, report that the plan could
  not be posted and that no work was done, then go to Finish; no feature
  branch is created.

**Status comment (router only).** When the workflow starts (or just
before), post one `<!-- grind:v1 status run=<run-id> -->` comment on the
meta issue with one line per child. Edit it with
`gh api -X PATCH repos/<o>/<r>/issues/comments/<id> -f body=...` as results
arrive and once more at Finish. clud's command hook reads a backtick as a
substitution even inside quotes, so write any body that has one to a file
and pass it with `--body-file <f>` (for the PATCH, `--input <f>` holding
`{"body": ...}`).

Concurrency is fixed by the workflow: at most 4 planner/worker/reviewer
agents at once, and exactly one integrator.

## 4b. Feature stage (feature-branch mode)

Only when the plan has feature children: after the bug-stage `grind-run`
call returns (section 4, step 1) and before the feature-stage call, so the
branch contains every bug fix that merged:

1. `git fetch origin <main>`.
2. Create exactly ONE worktree:
   `git worktree add <repo>/.clud/grind/worktrees/feature -b grind/meta-<M>-<run-id> origin/<main>`,
   where `<M>` is the meta issue.
3. With `preflight.action: carry`, apply the carried stash in that
   worktree (`git -C <worktree> stash pop <stash@{n} of grind-<run-id>-carry>`)
   and commit it as the branch's first commit,
   `grind: carry uncommitted changes from <starting branch>`. Then push the
   branch: `git -C <worktree> push -u origin grind/meta-<M>-<run-id>`.
4. Before the first goal lands, open a DRAFT feature PR. GitHub refuses a
   PR with no commits, and a branch just cut from `origin/<main>` has none
   unless step 3 carried changes, so first give it an opening commit:
   `git -C <worktree> commit --allow-empty -m "grind: open feature branch for run <run-id> (#<M>)"`
   and push it. Then
   `gh pr create --repo <github-repo> --draft --base <main> --head grind/meta-<M>-<run-id>`. The
   body starts with `Closes #<meta>` (plus `Closes #<original>` when intake
   converted an original issue), followed by a goals table (goal, goal PR,
   status). For a meta of metas, `<meta>` is the feature group's sub-meta;
   the top meta is never in a `Closes` line. The lander adds each goal's
   `Closes #N` line, updates the table and posts the issue's `grind:v1`
   marker comment as the goal lands.
5. Record `{"feature": {"branch": "…", "worktree": "…", "pr": "…"},
   "feature_merge": "…"}` in `<facts>`, and pass `feature` and
   `feature_merge` to the feature-stage `grind-run` call (section 4,
   step 2). Until this step `<facts>` has no `feature`: the bug stage runs
   under the plain caps, where the lander merges each bug PR into `<main>`.

The router creates no other worktree and never touches the user's checkout
after preflight. When `origin/<main>` moves during the feature stage, the
integrator merges it into the feature branch before the next feature goal
(`git merge --no-ff origin/<main>` in the feature worktree); nobody rebases
the feature branch. While `<facts>` records the feature,
clud's hook refuses the router a second `git worktree add`, any
`gh issue close --repo <github-repo>` (or `gh api … -f state=closed`), merging the feature PR,
and deleting a `grind/*` branch.

### Publication resumption

When a result contains `resume.schema: "grind-publication-resume/v1"`, the
run is waiting for publication. Retain the complete original approved
Workflow arguments and the actual `runId` and `scriptPath` returned by that
launch in `<facts>.orchestration_resume`, together with the returned manifest.
Never invent references or remove completed goals from the arguments.
Report the deferred goals and their gate evidence, then follow the publication
resumption section of `../grind-cron/SKILL.md`. Keep the facts, claims and
preflight record until resumption settles. Do not start the next stage, land
the feature PR, or run Finish cleanup while this manifest remains.

## 5. Finish (always, as the very last step)

After the last workflow call returns (both stage calls, when there are
two), whether or not every goal merged:

1. **Report** each goal's PR and its landing state: `merged`, `queued`
   (still in the merge queue after the final bounded wait; not a failure),
   `stuck` (dropped by the queue past its automatic fixups), `base-red` (the
   same required job fails on `<main>`), or `covered by #PR` (dedup
   preflight), and list anything left open with its reason, including each feature child blocked by a stuck
   bug (`blocked: bug #N did not land`) next to that bug. For a feature
   stage, by `feature_merge`:
   - `later`: when every feature goal landed, mark the feature PR ready
     (`gh pr ready --repo <github-repo> <fpr>`) but never merge it; report it as waiting for the
     user. If a goal did not land, leave it a draft and say which.
   - `comment`: post one result comment on the meta issue (branch, goals
     landed, feature PR) and keep the PR draft.
   - `auto`: report the feature PR as merged, or as "waiting for review".
     Once it merged, run `clud grind reconcile` again so every issue it
     closed gets its `resolved` tag.

   Then make the status comment final: PATCH the run's
   `<!-- grind:v1 status run=<run-id> -->` comment (section 4) so each
   child's line shows its outcome (merged with its PR, blocked, parked,
   deferred or failed with the reason) and none still reads `running`.
2. **File problems (router only).** Gather every `problems` item from each
   workflow result (each goal entry's `problems`, including the
   `goal: 'run'` entry for problems no goal owns, `feature.problems`, the
   top-level `problems`, and the plan-only pass's `problems` from 1b), and
   dedupe by kind + summary + related_issue. Then,
   by the plan's `problem_reporting`:
   - `issue`: for each problem, search for an existing follow-up with the
     same title
     (`gh issue list --repo <github-repo> --state all --search '"grind:followup" <summary> in:title,body'`)
     and skip duplicates; otherwise run
     `gh issue create --repo <github-repo> --title '<kind>: <summary>' --body-file <f>`,
     where the body holds the evidence, `Refs #<meta>`, and the marker
     `<!-- grind:followup meta=<meta> stage=<bugs|feature> feature-pr=<n or none> -->`
     (`stage=bugs` when the problem's goal was a bug-stage goal or the run
     has no feature stage). NEVER attach a follow-up as a sub-issue of the
     meta issue, and never add it to the meta's Tracks list.
   - `comment`: one `gh issue comment --repo <github-repo>` per problem on its `related_issue`,
     or on the meta issue when there is none, after the same duplicate
     search.

   If a create or comment fails, keep going, and list that problem inline
   (kind, summary, evidence) in the final report under "Problems not filed".
   Follow-ups never block the meta issue closing or the no-overlap rule,
   because they are not sub-issues.
3. **No files left behind.** Remove every
   worktree and temporary branch the run created, but only after checking
   it has no unpushed work (see `/clud-git`); push every worktree before
   removing it. Never delete a `grind/*` branch while its feature PR is
   open; Finish leaves those branches for the feature PR's merge. Never
   delete a `wip/grind-<goal>` park branch (a goal's `parked` result, #1424):
   it holds a rejected or failed goal's only copy; report it. A merged
   goal PR's commits stay reachable at `refs/pull/<n>/head`, but an
   unpushed worktree has no such copy, so push first. Only then remove
   `.clud/grind/plan.json` with `clud trash`, and last the run facts with
   `clud grind-facts clear` (last, because the hook reads them to guard
   `grind/*` branches during the cleanup). It removes only this session's
   file; another run's facts are never touched.
   Then `git status --porcelain`
   prints nothing: no untracked files, no uncommitted changes, and no stash
   the run created other than the preflight stash step 4 pops.
4. **Restore only what `preflight` recorded.** `git fetch origin`, then
   `git switch <preflight.branch>`; run `git pull --ff-only origin <main>`
   only when that branch is `<main>`. Then, by `preflight.action`:
   - `stash`: `git stash pop stash@{n}`, where `stash@{n}` is the entry
     named `grind-<run-id>` in `git stash list` (never a bare pop). If it
     conflicts, leave the stash in place and report it.
   - `wip`: leave `wip/grind-<run-id>` local, never pushed, and stay on
     the starting branch; report the branch name.
   - `carry`: nothing to restore; the changes are the feature branch's
     first commit. Report that.
   - `none`: nothing beyond the switch.

   Then list what the run left behind so it does not go stale unnoticed:
   `git branch --list 'grind/*' 'wip/grind-*' --no-merged origin/<main>`
   (salvage, park and unlanded branches, with their tips). Report them;
   never delete one.
5. **Report what you could not clean or restore**, and why. Never delete
   work the run did not create to get a clean status, and never ask about
   repo state here; report it instead.
6. **Release this run's checkout claim** with `clud claim release-own`.

## The DAG

| Node | Role | Caps |
|---|---|---|
| `/grind-intake` | router | main session |
| `/grind-prework` | `grind-prework` | read-only git/gh; `gh issue comment --repo <github-repo>` on the meta issue only; no edits, worktrees or builds |
| `/grind-plan` | `grind-planner` | read-only git/gh; `git worktree add` in parallel mode |
| `/grind-plan` (plan-only) | `grind-planner` | read-only git/gh; no Write/Edit, worktree or push |
| `/grind-work` | `grind-worker` | read/write assigned files; read-only `gh`; web search |
| `/grind-review` | `grind-reviewer` | same as worker |
| `/grind-integrate` | `grind-integrator` | full shell, one at a time; no `bosn`, containers only via `act`, no new worktrees |
| `/grind-land` | `grind-lander` | `gh pr`, `pr_merge_watch`, `pr_merge.py` (enqueue), `git push`; no edits |
| (workflow) | `grind-chaperone` | haiku; bounded `pr_merge_watch --until merged`, `gh run rerun --failed`, `pr_merge.py` re-enqueue; no checkout |
| `/grind-cron` | router | main session |
| Finish: file problems | router | main session; the only node that runs `gh issue create --repo <github-repo>` (`grind:followup` issues) |

Tools are capped by each agent type's `tools:` list; shell commands by
clud's PreToolUse hook, keyed on the agent type.

If a publication-resume manifest has `exhausted: true`, stop the native loop,
retain `orchestration_resume`, its launch references and the manifest, and
report the replay-history limit. Do not call Finish, advance its epochs or
restart completed work. The deferred goals remain unfinished.


Follow the [shared landing contract](https://github.com/talos-hq/clud/blob/main/docs/architecture/launch-plan.md#landing-preference) when delivering changes. Preserve this role's scope; required checks, review, merge queues, `--safe` and explicit user
instructions take precedence over any immediate admin-merge shortcut.

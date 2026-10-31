---
name: grind-plan
description: "Plan one /grind goal: pick its checkout, split it into worker tasks with disjoint files, declare dependencies on other goals, and name the integrator's verify commands. Read-only; never builds."
triggers:
  - When the grind workflow's planner starts a goal
  - When the user asks to plan a goal into disjoint worker tasks
disable-model-invocation: true
---
<!-- managed-by: clud -->

# /grind-plan

Use the prompt's **GitHub repository** (`OWNER/REPO`) as `<github-repo>`. Keep the checkout path separate. For direct use, resolve an explicit GitHub issue/PR URL or `git remote get-url origin` first; report an unresolved identity before running repository commands.

Keep tool output bounded (narrow searches, capped lists, ranged reads, bulk on disk, no identical repeats): https://github.com/zackees/clud/blob/main/docs/architecture/bounded-output.md

You plan one goal; you never build, lint or test. The integrator does that
later, one goal at a time.

1. **Read the goal.** If the brief is an issue, `gh issue view --repo <github-repo> <n>
   --comments`. Read the code it names and the repo's per-directory READMEs.
   **Dedup preflight (#2467).** Before planning, look for a PR that already
   covers the issue: `gh pr list --repo <github-repo> --state all --search
   'Closes #<n>' --json number,state,title,headRefName --limit 10` (also
   `#<n>` in a title and `*/<n>-*` branch names). If an open or merged PR
   already closes it, return `covered_by: "#<pr>"` with empty `tasks`; the
   workflow skips the goal and reports `covered by #<pr>`. A closed, unmerged
   PR does not count.
2. **Checkout and branch.** Branch `grind/<id>-<slug>`. `<base>` is the
   prompt's "Base branch": `<main>` for a bug-stage goal, the feature branch
   for a feature-stage goal.
   - Parallel mode: `git -C <repo> fetch origin <base>`, then
     `git -C <repo> worktree add --no-track ~/.clud/worktrees/<repo-name>-wt-<id> -b <branch> origin/<base>`
     (`--no-track` avoids concurrent planners writing shared Git config;
     the serialized integrator sets upstream tracking with its first push.)
     (reuse it if it exists; an older `<repo>-wt-<id>` sibling is reused
     too). The checkout is that worktree. `~/.clud/worktrees/` is clud's
     worktree root: the daemon reclaims a worktree there once its PR is
     squash-merged, or after 24 h if it was never used, and never on age
     alone.
   - Sequential mode: the checkout is the repository itself. Do not create a
     worktree or switch branches; the integrator creates the branch.
3. **Tasks.** One to eight tasks, each with a **disjoint** file set: two
   tasks never write the same file. Each task's instructions must be
   complete for a worker that cannot run anything: exact paths, what to
   change, what done looks like, which existing code to mirror, and the
   RED -> GREEN regression test to write for a code change.
   Never plan a task that only runs commands ("verify build and tests",
   "run clippy"): workers and reviewers cannot run anything, so it can only
   end "Not done". Every task writes at least one file; checks go in step 5.
   Record each task before returning, one call per task:
   `clud grind-facts task --checkout <checkout> -- <file>...`, with the
   checkout's absolute path and the task's repo-relative files. clud's hook
   then lets that task's worker and reviewer delete with `safe-rm`
   only under those files' directories.
4. **Dependencies.** `depends_on` lists other goals in this run that must
   land first (shared files, an API this goal consumes). Only goals listed
   before this one count; the workflow ignores any other. An empty list
   means the goal is isolated and rebases straight onto `origin/<base>`.
   Keep chains short; a dependent goal waits for its dependency to merge.
5. **Verify.** The exact lint, build and test commands the integrator runs,
   newest-first focused test before the broad gates, taken from the repo's
   own docs (for example `bash lint`, `bash test`). Name the `ci.yml` job
   to run under `act` only if local CI is on. When the prompt says the run
   has scripts (`./lint`, `./test`), the integrator already runs them before
   every push: do not invent lint or test commands; verify is just the
   focused test.

6. **Context pack** (#1339, #1344). Return `context`, once for the goal:
   `issue` is the goal issue's body and comments, verbatim, as you read them
   in step 1 (empty when the brief is not an issue); `excerpts` are the code
   ranges the workers and reviewer need, each `{path, start, end, why,
   text}` with `text` the exact lines `start..end` (1-based, inclusive).
   Prefer a few focused ranges over whole files, and stay under about
   400 lines in total. The workflow places this pack, byte for byte, at the
   start of every worker's and the reviewer's prompt, so they neither
   re-fetch the issue nor re-read these ranges, and the prompt cache serves
   it after the first agent. Put nothing task-specific in it.

## Platform coverage selection

Native PR coverage follows the changed platform implementation: native API/FFI
handling, OS branches, adapters/selectors or native runtime contracts. Shared
business logic and callers of unchanged platform code use the routine gate.
Name the implementation and select only affected targets; target inventories,
helper extraction and local coverage gaps do not justify `ci-full` or
`ci-windows`. Preserve routine checks and full release validation.

## Plan-only mode

When the prompt says PLAN-ONLY, skip steps 2-5 and only classify:

- Read each child (`gh issue view --repo <github-repo> <n> --comments`) and the code it names.
  Run one plain command per call: clud's hook refuses `cd`, shell loops
  and command substitution for the planner, so read the children one by
  one. If a read is refused, try a plainer form; classify from titles
  alone only as a last resort, and then set `confident` to false and report
  the refused command as a `tooling` problem.
- Keep it short: classification reads the issues and only the code needed
  to place each child, not a whole plan. A planner that never returns
  stalls the run, and nothing times it out.
- Classify each child as a **bug** (a self-contained fix to existing
  behaviour, independent of the other children, landing as its own PR into
  the default branch) or a **feature** (one part of a single cohesive
  change). A lone child is always a bug: one change is not a part of one.
  Set `read` to true for each child whose issue you actually read. The
  workflow moves a lone child, or a feature with `read` unset, to the bug
  track, so a title-only guess cannot send a fix down the feature path (#1830).
- Name the feature groups and say whether each is independent of the others.
- List which feature children depend on which bugs (`depends_on_bugs`), and
  give a dependency order for all children.
- Return every child exactly once, with its id as the prompt gives it.
- Set `confident` to false if any child cannot be placed or the split is a
  guess.

Never write files, create worktrees or branches, or push; clud's hook refuses
them while the run facts have `"phase": "plan"`. Return the classification
through StructuredOutput.

Otherwise, return the plan through StructuredOutput.

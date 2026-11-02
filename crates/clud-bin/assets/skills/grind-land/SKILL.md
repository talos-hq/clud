---
name: grind-land
description: "Land one /grind PR: watch its checks with pr_merge_watch, enqueue it when green (the goal is done at enqueue; a chaperone follows the queue), or diagnose the failure and hand it back to the integrator. Never edits or builds."
triggers:
  - When the grind workflow lands a pushed PR
  - When the user asks to watch a PR and merge it once green
disable-model-invocation: true
---
<!-- managed-by: clud -->

# /grind-land

Use the prompt's **GitHub repository** (`OWNER/REPO`) as `<github-repo>`. Keep the checkout path separate. For direct use, resolve an explicit GitHub issue/PR URL or `git remote get-url origin` first; report an unresolved identity before running repository commands.

Keep tool output bounded (narrow searches, capped lists, ranged reads, bulk on disk, no identical repeats): https://github.com/zackees/clud/blob/main/docs/architecture/bounded-output.md

The integrator already proved RED -> GREEN locally; you confirm it on the
server and enqueue. **You end at enqueue** ([DD-192](https://github.com/talos-hq/clud/blob/main/docs/DESIGN_DECISIONS.md#dd-192-workers-end-at-enqueue-and-a-chaperone-owns-the-merge), #2467):
never wait for the merge and never rebase before landing; the queue's
`merge_group` run tests the merged tree and a real conflict shows up as an
ejection, which the workflow's `grind-chaperone` reports back.

1. **Choose the wait budget and scope.** Check the declared execution tools
   and the slowest selected CI lane, including queue/setup time. With a
   persistent session or supported background completion, a typical bounded
   watch is `clud tool run github/pr_merge_watch.py -- <n> --repo <github-repo> --timeout 1080 --require '.*'`.
   Keep its deadline within the verified clud wrapper/runtime limit. With
   foreground execution only, choose a deadline below that tool's actual cap;
   do not assume every environment has a 600-second cap. A long-running lane
   may need several bounded observations of the same CI.

   Start once. Wait on the same live process with the declared session-wait
   tool, or the supported background completion notification. Never restart
   a live session. Run the waiter bare; its exit code is the result (do not
   pipe it through `tail`). The shared capability and outcome contract is
   [watch recovery](https://github.com/zackees/clud/blob/main/docs/architecture/hook-dispatch.md#watch-recovery-and-continuation-2004).
   `--require '.*'` selects every reported check, including advisory lanes;
   without it, green covers branch-required checks only. Preserve the same
   repository, PR, head and selected scope when resuming. Add `--no-cancel`
   when the user's policy requires keeping runs intact.
2. By exit code:
   - `0` (checks green, mergeable): satisfy required review, resolve the full
     head SHA, then submit with `clud tool run github/pr_merge.py -- <n>
     --repo <github-repo> --match-head-commit <head-sha>`. This honors a
     required merge queue. Return `status=enqueued` with `head_sha` at once;
     do not run `--until merged` (the workflow's chaperone does). Return
     `status=merged` only when pr_merge.py itself reports the PR merged (a
     base with no queue). The watch never waits for
     CodeRabbit unless you pass `--coderabbit-wait N`; never pass it for a
     repo that suppresses CodeRabbit or a head validated by the local
     bosn gate. Every `coderabbit=` note (`suppressed`, `absent`,
     `completed`, `rate_limited`, `skipped`, `pending`, `timeout`) is still
     `0`: merge.
   - `1` (a required check failed): `gh pr checks --repo <github-repo> <n> --json name,state --jq '[.[]|select(.state!="SUCCESS" and .state!="SKIPPED")]'`
     (unresolved checks only; see [Reading CI status](https://github.com/zackees/clud/blob/main/docs/architecture/ci.md#reading-ci-status-without-flooding-context-2312)) and
     `clud tool run github/ci_failure_report.py -- --repo <github-repo> --run <run> --job <job>`
     for the failing job. Use the returned artifact and line-range continuation to inspect more.
     **Red-base breaker (#2467):** compare each failing required job with
     `<main>`'s latest `push` and `merge_group` runs (`gh run list --repo
     <github-repo> --branch <main> --limit 5 --json databaseId,event,conclusion`,
     then `gh run view --repo <github-repo> <id> --json jobs`). If the same job
     fails there too, no diff can fix it: return `status=base_red` naming the
     job and main's run, with no rerun. Otherwise return
     `status=needs_fix` with the failing job and the relevant log lines in
     `failure_log`.
   - `2` (new review activity): read the unresolved comments with
     `clud tool run github/pr_context.py -- --repo <github-repo> --pr <n>`, then continue with
     its `--section review-comments` / `--section reviews` offsets and the private snapshot.
     Do not use `gh pr view --comments`: it prints every review and thread body at once.
     Return `status=needs_fix` with each actionable comment in `failure_log`.
   - `3` (closed or merged elsewhere): report it; `status=merged` only if
     `gh pr view --repo <github-repo>` says merged, else `gave_up`.
   - `4` (timeout): incomplete observation. Revalidate the same CI and head;
     after this waiter exits, continue a bounded watch while CI is healthy
     and progressing. Do not rerun CI or spend a fix round because a watch
     deadline expired. Repeated observations alone are not a CI failure.
     If a verified external runtime limit prevents continuing, return
     `status=gave_up` with an explicit incomplete summary and the repository,
     PR, head, live CI run IDs, scope and resume command. Never claim green
     or permanent goal blockage from a timeout.
   - `124` with JSON `status: in-progress` (clud's resumable watchdog):
     the wrapper stopped the waiter. Reconcile the same CI, then use the
     same selector and arguments; never restart CI or treat this as green.
     A live tool session is still running: keep waiting on it instead.
     Handle a verified external runtime limit as for `4`. Exit `124`
     without that JSON is an error; report the diagnostic rather than merge.
   - `5` (approval required: a fork PR's runs are `action_required`): stop
     and report it to the user; do not retry. Return `status=gave_up`.
   - `6` (never reported: a required check never got a check run although
     every run finished, likely path or branch filters, or a `[skip ci]`
     marker on a branch with required checks): report it as blocked. Return
     `status=gave_up`.
   - `7` (stale: a check is older than 14 days): re-run its workflow once,
     e.g. `gh run rerun --repo <github-repo> <id>`, then watch again; a second `7` is `gave_up`.
   - `8` (NO_CHECKS: no workflows, a `[skip ci]`-style marker on the head
     commit, or no check registered within the grace period): grind never
     merges an unvalidated head. First list the base branch's workflows:
     `git ls-tree -r --name-only origin/<base> -- .github/workflows`.
     - Empty (the repo has no workflows at all): read the final line's
       `mergeStateStatus`. `CLEAN` means merge now, as for `0`. Anything
       else (`BLOCKED`, `BEHIND`, ...): report it and return
       `status=gave_up`.
     - Not empty, and the head commit or PR carries a skip marker
       (`[skip ci]`, `[ci skip]`, `[no ci]`, `[skip actions]`,
       `[actions skip]`, `skip-checks: true`): never merge. Return
       `status=needs_fix` asking the integrator to reword the message
       without the marker and push again.
     - The final reason is `no_trigger_for_base` (#1893: workflows exist
       but none triggers on a PR into this base, e.g. a `branches:` filter
       on `pull_request`): a push cannot help, so never return
       `status=needs_fix`. Apply the run's recorded fallback from the plan
       (dispatch CI with `gh workflow run <wf> --ref <head-branch>` and
       watch that run with `"$CLUD_EXE" tool run github/run_watch.py`), or
       return `status=gave_up` naming the gap.
     - Not empty and the rollup stayed empty: never merge. Return
       `status=needs_fix` (push again to trigger CI) on the first `8`, and
       `status=gave_up` on a second.
     Remote CI is never skipped: the run's Local CI choice controls local
     `bosn` replay on this machine only (#1429).
   - `9` (conflict): when the final reason is `conflict`
     (`mergeable=CONFLICTING` / `mergeStateStatus=DIRTY`), return
     `status=needs_fix` asking the integrator to rebase onto the base branch.
     When it is `mergeable_unknown`, watch once more first; a second `9` is
     `needs_fix`.
   - `10` (GitHub unreachable: auth, rate limit or network; the stderr is
     printed): run `gh auth status`, retry the watch once, then `gave_up`
     with the stderr. Never report it as closed.
   - `11` (queued too long; only with `--max-queued`): report the queued
     jobs and runner labels. Return `status=gave_up`.
   - `64` (usage error: a bad flag, e.g. `--pr`; the PR number is
     positional): your own mistake, never a verdict. Fix the command and
     run it again in this round; it never counts as `needs_fix` or spends a
     fix round (#1331). Likewise `command not found` and a wrong tool path.
     Only if you still cannot run the tool correctly after fixing it twice,
     return `status=tool_misuse` with the failing command in `command`: the
     workflow lands again on its own small allowance and never counts it as a
     fix round. A `4` (timeout) is not misuse: watch again.
   - A cancelled check superseded by a newer run of the same workflow is
     ignored, so concurrency cancellations are not failures.
3. **Feature stage.** A goal PR in the feature stage has the feature branch
   as its base and merges into it only, never into `<main>`: on exit `0`
   use `gh pr merge --repo <github-repo> <n> --admin --merge` instead of step 2's command. No
   `--delete-branch` on any merge while the feature PR is open. Right after a
   goal PR merges into the feature branch, record it so the issue is never
   lost:
   1. `gh issue comment --repo <github-repo> <N> --body '...'` with a readable line (`Landed on
      feature branch <feature-branch> via #<gpr>; closes when feature PR
      #<fpr> merges.`) followed by the hidden marker, exactly
      `<!-- grind:v1 feature-pr=#<fpr> branch=<feature-branch> goal-pr=#<gpr> run=<run-id> -->`.
      On the meta (and original) issue, first read its comments
      (`gh issue view --repo <github-repo> <m> --json comments`); only if none carries a marker
      with `feature-pr=#<fpr>`, post the same marker without `goal-pr`.
   4. `gh pr view --repo <github-repo> <fpr> --json body`, then `gh pr edit --repo <github-repo> <fpr> --body '...'`
      with `Closes #<N>` added and the goals table (goal, goal PR, status)
      updated; keep every other line. You have no file tools, so pass the
      body inline. View the body again and repeat the edit if `Closes #<N>`
      is missing: in parallel mode another lander may have edited it at the
      same moment. Only you add a goal's `Closes` line, and only after its
      goal PR merged, so a goal that never lands never closes.
   Never `gh issue close --repo <github-repo>`: the feature PR's `Closes` lines close the issues
   when it merges into `<main>`, and clud's hook denies the command. The
   feature PR
   itself is merged only when the run's `feature_merge` is `auto`: once
   every feature goal has landed and its checks are green, `gh pr ready --repo <github-repo>
   <feature-pr>` then `gh pr merge --repo <github-repo> <feature-pr> --merge`; never `--admin`,
   `--squash` or `--delete-branch` on it. If required review blocks the
   merge, stop and return "waiting for review". Under `later` or `comment`
   leave the feature PR alone. clud's command hook enforces these rules for
   the lander.
4. You cannot edit or build. The workflow gives a `needs_fix` to the
   integrator and calls you again, up to 10 rounds. Each fix round goes back
   through the integrator, which reruns the run's lint and test scripts
   before pushing.

## Platform coverage selection

Native PR coverage follows the changed platform implementation: native API/FFI
handling, OS branches, adapters/selectors or native runtime contracts. Shared
business logic and callers of unchanged platform code use the routine gate.
Name the implementation and select only affected targets; target inventories,
helper extraction and local coverage gaps do not justify `ci-full` or
`ci-windows`. Preserve routine checks and full release validation.


Follow the [shared landing contract](https://github.com/talos-hq/clud/blob/main/docs/architecture/launch-plan.md#landing-preference) when delivering changes. Preserve this role's scope; required checks, review, merge queues, `--safe` and explicit user
instructions take precedence over any immediate admin-merge shortcut.

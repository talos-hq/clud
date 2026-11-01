---
name: grind-integrate
description: "Integrate one reviewed /grind goal: commit, rebase onto its base, lint, build and test until green, optionally replay ci.yml jobs under bosn, then push and open the PR, or park a failed sequential goal's changes on a local wip branch. The only grind role that builds; runs one at a time."
triggers:
  - When the grind workflow integrates a reviewed goal
  - When the grind lander hands back a failing PR for a fix round
disable-model-invocation: true
---
<!-- managed-by: clud -->

# /grind-integrate

Use the prompt's **GitHub repository** (`OWNER/REPO`) as `<github-repo>`. Keep the checkout path separate. For direct use, resolve an explicit GitHub issue/PR URL or `git remote get-url origin` first; report an unresolved identity before running repository commands.

Keep tool output bounded (narrow searches, capped lists, ranged reads, bulk on disk, no identical repeats): https://github.com/zackees/clud/blob/main/docs/architecture/bounded-output.md

You hold the run's build lock, so nothing else builds while you do.

1. **Branch.** In the checkout given:
   - Parallel: the worktree is already on the goal's branch.
   - Sequential: `git fetch origin <base>`, then
     `git switch -c <branch> origin/<base>` (`<base>` as in step 2), carrying the worker edits
     (`git stash` first if the switch needs it).
   Commit the goal's files by name (never `git add -A`) with a conventional
   message naming the issue.
2. **Rebase** onto `origin/<base>` after a fresh fetch, where `<base>` is
   `<main>` in the bug stage and the feature branch
   (`grind/meta-<M>-<run-id>`) in the feature stage. A dependent goal's
   dependency has already merged, so `origin/<base>` contains it. In the
   feature stage, first check whether `origin/<main>` moved past the feature
   branch; if so, in the feature worktree fast-forward to the feature
   branch's origin (`git merge --ff-only origin/<feature>`), merge (never
   rebase) `origin/<main>` into it with `git merge --no-ff`, and push it with
   a plain push, then rebase the goal onto the updated feature branch.
3. **RED -> GREEN.** Run the goal's focused regression test and show it
   fails without the fix (check out the test alone on the base, or cite the
   reproduction), then passes with it.
3b. **Cross-check every target the project's CI tests** only when the diff
   changes platform implementation code (Rust repos; scopes #1430).
   First inspect the changed code: native API/FFI handling, OS-specific
   branches, platform adapters/selectors and native runtime contracts
   qualify. Shared business logic, callers of unchanged platform APIs,
   helper extraction and moving OS code unchanged do not. For those changes,
   proceed to step 4 without adding cross-target passes or native CI labels.
   Keep the repository's declared routine lint/check/test gate.

   For a platform implementation change, run
   `clud tool run git/ci_targets.py --host <host triple>` to inventory
   available targets, then select only targets exercising the changed
   platform implementation. The inventory includes release-only targets;
   it is not a requirement to run them all. Record the implementation,
   selected targets and native behavior being checked. A non-Rust repo
   prints `not a Rust project`: skip this stage. Report an affected target
   under `skipped` and its reason rather than claiming coverage.

   Per affected target, use the repository's isolated validation runner and
   declared platform-crate commands, with warnings denied. If the platform
   implementation is in a mixed workspace without a narrower check, the
   command is `soldr cargo check --workspace --all-targets --target <triple>`;
   otherwise scope it to the platform crate with `-p <platform-crate>`.
   Use the repo's CI feature set and lint flags. Do not duplicate its
   routine Dylint/cross-target checks with a second ad hoc loop.
   Here "every target" means every affected selected target, not every
   target found in the inventory. Host lint and test (step 4) run only after every target passes.
   A failure must be fixed or reported with evidence, never silently dropped.
   Apply the same implementation-based selection to `ci-windows`/`ci-full`;
   a local coverage gap or a platform caller alone never justifies a label.
4. **Verify.** Run the plan's lint, build and test commands. Fix failures by
   editing, commit, and rerun until green. Do not skip or weaken a test.
   **Run must_verify.** The reviewer cannot run anything, so the checks it
   lists under `must_verify` arrive in your verify commands: run every one
   before pushing. A failure there is a real defect: fix it.
   **Run scripts.** When the prompt lists the repo's `./lint` / `./test`,
   run lint before test before every push, fix rounds included, after the
   focused test:
   - A script likely to exceed the 600s Bash cap runs with
     `run_in_background`; judge it by its exit code, not by its output.
   - After any fix, rerun both scripts until both are green.
   - If a script fails on untouched `origin/<main>` too, fix it and commit
     that fix separately as `fix: pre-existing lint failure` (or
     `fix: pre-existing test failure`), apart from the goal's commit.
   **After a failed run: read, then decide.** A deterministic failure does
   not go away when rerun, so:
   - **Read before retrying.** After any failed lint or test run, read the
     failing test names and their errors first. Never rerun a suite you have
     not read the failure of.
   - **Retry only infrastructure errors.** Rerun unchanged only for a known
     infrastructure error (for example `SESSION relay closed before Exit`,
     or a network or registry timeout), at most twice. Never wrap a full
     suite in a retry loop (`for i in 1 2 3; do bash test; done`); clud's
     hook refuses one.
   - **Same failure twice is real.** If the same test fails on two
     consecutive runs, it is not flaky: fix it, or report it as pre-existing
     with evidence, namely that it also fails on untouched `origin/<main>`.
   - **Wait by condition.** Wait on a background run by its exit (the
     completion notice, or an exit code or marker file it writes), never in
     fixed 540–600 s sleep blocks.
5. **Local CI**, only when on: run the relevant `ci.yml` jobs under bosn:
   `bosn ci run --workspace . --trigger pr --job <job> --wait` (or
   `clud ci [JOB] [--filter RE]`). Run the full PR plan at most once per
   change, and prefer focused `--job` runs. Read the verdict with
   `bosn ci report RUN`. If the run stays `queued` for 2 minutes, cancel
   it with `bosn ci cancel RUN` and report "local CI skipped: bosn daemon
   busy". Stop only your own run: never `pkill`/`pgrep` bosn. Do not call
   `act` or start containers yourself. This is the only CI a "skip CI"
   choice skips: never put `[skip ci]` (or any skip marker) in a commit or
   PR text, and never remove a CI-lane label; remote CI always runs.
   This does not mean adding optional native coverage to every PR. Select
   platform labels only by the changed implementation described in step 3b.
   If an existing label contradicts that selection, report the mismatch
   for owner correction; do not add further labels to compensate.
6. **Review gate.** Run `/clud-review` on source-code changes.
7. **Push and PR.** `git push -u origin <branch>`, then `gh pr create --repo <github-repo>` with
   `Closes #<id>` for an issue goal only when the PR's base is the
   repository's default branch; when the base is any other branch (e.g. a
   feature branch) use `Refs #<id>` instead, because GitHub only auto-closes
   on merges into the default branch and a premature close would lose the
   issue. Either keyword names the goal's own issue, never the parent of a
   meta issue. So feature-stage goal PRs (base = the feature branch) use
   `Refs #N`. Do not edit the feature PR: the lander adds the goal's
   `Closes #N` line to it only after the goal PR merges into the feature
   branch, so a goal that never lands is never closed by the feature merge.
   Return `pushed=true` and the PR URL. The lander watches CI; do not wait
   for it here. **Remote-only verification:** when a verify command can only
   run in remote CI (a native lane), push, then run
   `clud tool run github/pr_merge_watch.py <PR> --timeout 540` with
   `run_in_background` and judge by its exit code; on exit 1 read the failing
   job's log from its report. Never `sleep N; gh run view` (#1894). For a one-off read ask only for unresolved
   jobs: [Reading CI status](https://github.com/zackees/clud/blob/main/docs/architecture/ci.md#reading-ci-status-without-flooding-context-2312).

**Queue fixup (#2467).** When the prompt says `QUEUE FIXUP`, the merge queue
ejected an already-enqueued PR (or it now conflicts). Switch to its branch,
fetch, rebase onto `origin/<base>`, fix what the chaperone reported, rerun the
verify commands, push with `--force-with-lease`, return the checkout to
`origin/<base>`, and return `pushed=true`. The lander then re-enqueues it.

**Fix round.** When the prompt carries a failure from the lander: read it,
reproduce locally when possible, fix, rerun the verify commands (and the run
scripts, lint then test), commit and
push to the same branch. Return the same PR URL.

On a failure you cannot fix, return `pushed=false` with the failing command
and its last 60 lines in `failure_log`.

**Park (sequential mode, when the prompt says `PARK goal`).** The goal was
rejected, blocked or failed before its work was pushed, and its changes must
leave the shared checkout before the next goal starts. Do not commit to the
goal branch, push, open a PR, or run lint or test.

1. In the checkout given, run `git status --porcelain`. The goal's changes
   are the goal files the prompt lists plus any other path changed since the
   run started. The user's pre-run state, as the prompt's preflight records
   it, is never the goal's: with `carry`, the carried changes in the feature
   worktree stay put. With `stash`, `wip` or none recorded, the checkout was
   clean when the run started.
2. If any of the goal's paths are changed: `git switch -c <park branch>`
   (the uncommitted changes come along; from a goal branch its commits do
   too), `git add -- <those paths>` by name, never `git add -A`, then
   `git commit -m "wip(grind): park goal <id>"`. The park branch
   `wip/grind-<goal>` stays local: never push it.
3. `git fetch origin <base>`, then `git switch --detach origin/<base>`.
   Never `git reset --hard`, `git clean`, `git checkout -- .` or
   `git stash drop`: they destroy what is not the goal's.
4. Run `git status --porcelain` again. Return `parked` (whether you
   committed to the park branch), `branch`, `files`, and `clean=true` only
   when none of the goal's paths is still listed.


Follow the [shared landing contract](https://github.com/talos-hq/clud/blob/main/docs/architecture/launch-plan.md#landing-preference) when delivering changes. Preserve this role's scope; required checks, review, merge queues, `--safe` and explicit user
instructions take precedence over any immediate admin-merge shortcut.

---
name: grind-integrate
description: "Integrate one reviewed /grind goal: commit, rebase onto its base, focused test and multi-platform lint gate until green, optionally replay ci.yml jobs under bosn, then push, open the PR and return, or park a failed sequential goal's changes on a local wip branch. One at a time; ends at push."
triggers:
  - When the grind workflow integrates a reviewed goal
disable-model-invocation: true
---
<!-- managed-by: clud -->

# /grind-integrate

Use the prompt's **GitHub repository** (`OWNER/REPO`) as `<github-repo>`. Keep the checkout path separate. For direct use, resolve an explicit GitHub issue/PR URL or `git remote get-url origin` first; report an unresolved identity before running repository commands.

Keep tool output bounded (narrow searches, capped lists, ranged reads, bulk on disk, no identical repeats): https://github.com/zackees/clud/blob/main/docs/architecture/bounded-output.md

You hold the run's build lock, so nothing else builds while you do.

1. **Branch.** In the checkout given:
   - Parallel: the worktree is already on the goal's branch.
   - Sequential: record `git rev-parse HEAD` as `<starting-head>`, then
     `git switch -c <branch>` at the current HEAD, carrying the worker edits.
     Commit before fetching and rebasing in step 2; do not switch dirty edits
     directly onto a different base.
   **Never mutate the stash stack.** Only `git stash list` is permitted;
   preflight/carry stashes belong to the router, never the integrator.
   Commit the goal's files by name (never `git add -A`) with a conventional
   message naming the issue.
2. **Rebase** after a fresh fetch. Sequential mode uses
   `git rebase --onto origin/<base> <starting-head> <branch>` to transplant
   only this goal's commits: the previous goal may still be awaiting merge.
   Parallel mode rebases normally onto `origin/<base>`. Here `<base>` is
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
   **Formatter output.** Before lint, record the changed paths and their diffs.
   After lint, inspect the diff again. Commit formatting needed by the goal,
   naming its files explicitly. For unrelated formatter-only changes, use
   `git restore -- <explicit paths>` only when those paths were clean before
   lint and the diff proves the changes came solely from that lint run.
   Never restore a whole tree, a pre-existing edit or carried user work. If
   attribution is uncertain, stop and report the paths instead of discarding
   or stashing them. Resolve this output before switching goals or pushing.
3b. **Multi-platform lint gate, required before every push (DD-205).** In a
   Rust workspace that declares `[workspace.metadata.soldr].targets`, run
   Clippy and Dylint for every declared target through soldr: `soldr lint rust`
   covers cross-target Clippy; Dylint uses the repo's lint script or CI recipe,
   normally `soldr dylint prepare --target <triple>` then
   `soldr dylint --all -- --workspace --all-targets --target <triple>`. A
   finding blocks the push: fix it. If a target's toolchain cannot be
   provisioned on this machine, name it in your summary rather than claiming
   coverage. A non-Rust repo or one with no declared targets uses its plain
   lint. Native CI labels follow the changed platform implementation alone;
   this gate is lint, not coverage, and never justifies a label.
4. **Verify.** Run the plan's focused commands. Fix failures by
   editing, commit, and rerun until green. Do not skip or weaken a test.
   **Run must_verify.** The reviewer cannot run anything, so the checks it
   lists under `must_verify` arrive in your verify commands: run every one
   before pushing. A failure there is a real defect: fix it.
   **Run the lint script.** When the prompt lists the repo's `./lint`, run
   it before every push, after the focused test. Full test suites belong to
   required CI, not to you:
   - A script likely to exceed the 600s Bash cap runs with
     `run_in_background`; judge it by its exit code, not by its output.
   - After any fix, rerun it until green.
   - If it fails on untouched `origin/<main>` too, fix it and commit that fix
     separately as `fix: pre-existing lint failure`, apart from the goal's commit.
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
   `Refs #N`. Do not edit the feature PR: its chaperone adds the goal's
   `Closes #N` line to it only after the goal PR merges into the feature
   branch, so a goal that never lands is never closed by the feature merge.
   Return `pushed=true`, the PR URL, `head_sha` and a compact `handoff` for
   the chaperone (what changed and why, what you verified, what is fragile)
   immediately. Never wait for CI: a verify command that can run only in remote
   CI goes in the handoff as a check for the chaperone to read. For a one-off read ask only for unresolved
   jobs: [Reading CI status](https://github.com/zackees/clud/blob/main/docs/architecture/ci.md#reading-ci-status-without-flooding-context-2312).

**Later failures are not yours.** After your push a chaperone owns the PR:
red checks and queue ejections are fixed by that chaperone in its own worktree
(DD-205), so a push is your last act on a goal.

On a failure you cannot fix before pushing, return `pushed=false` with the
failing command and its last 60 lines in `failure_log`.

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

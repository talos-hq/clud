---
name: clud-fix-quick
description: Local-first speed mode. Edits in place on the current branch, runs lint + targeted test + /clud-review on source-code changes only, skips all three for docs/config/SKILL.md. Pushes direct; falls back to a PR when the branch is protected, merging immediately only where repository policy permits, then observes hosted validation. Trades rigor for iteration speed; escalate to /goal when the change is large or risky.
triggers:
  - When the user says "/clud-fix-quick <issue-or-task>"
  - When the user asks to "quickly fix" / "just push" / "iterate fast" / "speed mode"
  - When the user wants a small, well-understood change shipped without the full worktree/CI scaffold
  - When iterating on docs, SKILL.md, README, or other non-source files
---
<!-- managed-by: clud -->

# /clud-fix-quick

Before the first edit, run `clud claim acquire` in the current checkout.
If the daemon is unavailable or another session holds the claim, stop and
offer a new worktree or a retry after the claim releases.

Speed mode for small, well-understood changes. Edits files in place on the current
branch (no disposable worktree), runs the smallest set of gates that
still catches regressions, then pushes — directly when the branch
allows, or via a PR that gets admin-merged immediately only where policy permits.
Required checks, reviews and merge queues take precedence. The targeted-test step (when source files change) is the
GREEN signal that the change works; this skill delegates the RED ->
GREEN cycle to that focused test rather than running a full ritual,
trading some rigor for iteration speed.

Not a replacement for `/goal` — lock in a deliverable and drive to it
when the change matters or when the user wants a paper trail through
CI. Use
`/clud-fix-quick` when you already know what to change and want it on
main / your branch in seconds.

## Platform coverage selection

Native PR coverage follows the changed platform implementation: native API/FFI
handling, OS branches, adapters/selectors or native runtime contracts. Shared
business logic and callers of unchanged platform code use the routine gate.
Name the implementation and select only affected targets; target inventories,
helper extraction and local coverage gaps do not justify `ci-full` or
`ci-windows`. Preserve routine checks and full release validation.

## Input

- A GitHub issue URL or number — the task to implement.
- A bare description of a small fix when no issue exists (e.g.
  `/clud-fix-quick fix the typo in docs/architecture/crash-reports.md
  line 27`).
- Optional flags:
  - `--no-review` skips the `/clud-review` step on source changes
    (use sparingly — for mass typo fixes, generated-code updates,
    revert-style commits where review can't help).
  - `--no-test` skips the targeted test (use even more sparingly —
    only for changes that genuinely have no exercisable test).

For multi-forge inputs, the same forge-recognition rules used
elsewhere apply — read the URL prefix
or infer from `git remote get-url origin` for bare-number inputs.

## Hard Rules

1. **Current-branch warning is non-skippable.** If the active branch
   (`git rev-parse --abbrev-ref HEAD`) is not the repo's default
   branch, print the following warning verbatim in ALL CAPS at the
   top of the run, before any edit:

   ```
   ⚠ WARNING: COMMITTING DIRECTLY TO FEATURE BRANCH `<branch-name>`.
   THIS IS NOT A DISPOSABLE WORKTREE — A BAD EDIT WILL LIVE ON YOUR
   BRANCH. PROCEED ONLY IF YOU ARE SOLE AUTHOR OF THIS BRANCH.
   ```

   Continue the run; the warning is a heads-up, not a gate.

2. **No full test suite.** Run only the targeted test that exercises
   the change. Targeted-test selection rules per language:
   - **Rust**: `SOLDR_PORCELAIN=1 soldr cargo test -p <crate> --lib
     <module>::<test-name>` or `--test <file>` matching the changed file.
     That prints ONE JSON line (verdict, `exit_code`, counts, `log_path`);
     read the `log_path` file only when `exit_code` is non-zero. If stdout
     is not a JSON object, the installed soldr predates the mode - read the
     output normally. Never wrap the call in a grep/sed filter.
     If several crates need targeted tests, use one
     `SOLDR_PORCELAIN=1 soldr cargo test -p a -p b` call with the targeted filters.
   - **Python**: `pytest -k <pattern>` matching the changed file or
     test name.
   - **JS/TS**: `npm test -- -t <pattern>` matching the changed file
     or test name.
   - **Shell**: no test framework standard; skip with a one-line
     notice and continue.

   If no targeted test exists for the changed file, print
   `no targeted test found for <file>; proceeding without regression
   check` and continue. Do NOT fall back to the full suite — that
   would defeat the whole point of this skill.

3. **Lint scoped to the touched files.** Use `bash lint --files
   <changed-files>` if the script supports per-file linting;
   otherwise run the project's default lint and accept the time cost.
   Lint failure blocks the push.

4. **`/clud-review` runs on source-code changes only.** Skip for
   docs/config/SKILL.md/markdown. When it runs, use
   [[clud-review]]'s delegated-mode contract — it
   returns structured findings and the agent decides whether to
   address CRITICAL/HIGH ones before pushing. MEDIUM/LOW are
   advisory.

5. **No worktree, no `/goal`, no Stop hook installation.** This skill
   doesn't own a goal; it just does the work in the current checkout
   and reports.

6. **Direct push first, PR + admin-merge fallback second.** Try
   `git push origin <current-branch>`. If it succeeds, surface the
   commit SHA and stop. If it fails with `protected branch` / `remote
   rejected`, open a PR (`gh pr create --base <default-branch>`),
   then `gh pr merge <num> --admin --squash --delete-branch`
   immediately — no `--auto`, no waiting for CI. The skill trusts
   the local gates that already passed (lint + targeted test +
   `/clud-review`).

7. **Never `--no-verify`, never skip hooks.** Hook failures are real
   signals; fix them, don't bypass them.

8. **No force-push, ever.** A clean non-fast-forward push race is
   recovered with the Push-Race Recovery procedure below (fetch,
   normal rebase, cheap re-check, one normal retry); anything beyond
   that clean case — a rebase conflict, unexpected local changes, or
   a second rejection — is surfaced and stops. Speed mode doesn't
   mean destructive.

## Target Classification

The skill classifies the touched files and runs the matching gates.
Mixed-class changes run the union (so any source file in the diff
triggers all three source gates).

| File extensions / paths | Class | Lint | Targeted test | `/clud-review` |
|---|---|---|---|---|
| `.rs`, `.py`, `.ts`, `.tsx`, `.js`, `.jsx`, `.go`, `.c`, `.cpp`, `.h`, `.hpp`, `.java`, `.kt`, `.swift` | source | yes | yes | yes |
| `.sh`, `.bash`, `.ps1` | source (shell) | yes (shellcheck if available) | none | yes |
| `Cargo.toml`, `pyproject.toml`, `package.json` | source-adjacent | yes (cargo check / pip check / npm install --dry-run) | yes (touched crate's tests) | yes |
| `.md`, `.txt`, `.rst`, `.adoc` | docs | no | no | no |
| `.yaml`, `.yml`, `.toml`, `.json`, `.ini` (NOT package manifests) | config | no | no | no |
| `SKILL.md`, `CLAUDE.md`, `AGENTS.md`, `README.md` | docs (skill/guidance) | no — but RUN the bundled-skill guardrail tests (`skills::`) if any `crates/clud-bin/assets/skills/*/SKILL.md` was touched | bundled-skill guardrails | no |
| `.github/workflows/*.yml`, `.github/instructions/*.md` | CI / instructions | no | no | no |
| everything else | other | no | no | no |

A polyglot change touching `.rs` and `.md` files is a source change
for review purposes; run the source gates.

## Workflow

1. **Branch check + warning.** Run `git rev-parse --abbrev-ref HEAD`.
   Compare against the default branch (`gh repo view --json
   defaultBranchRef --jq .defaultBranchRef.name`). If they differ,
   print the ALL CAPS warning from Hard Rule 1.
2. **Read the task.** Resolve the issue URL/number via `gh issue
   view` (or the equivalent native CLI per the forge-recognition
   rules), or use the freeform sentence directly.
3. **Implement in place.** Edit files directly in the current
   checkout — no worktree, no branch creation, no `.claude/`
   directory work.
4. **Classify the touched files.** Use the table above. Compute the
   set of gates to run as the union across all touched files.
5. **Lint** (if any source file in the diff). Block on failure.
6. **Targeted test** (if any source file in the diff). Block on
   failure. If `--no-test` was passed AND the change set is small
   AND no targeted test exists, skip with the documented one-line
   notice.
7. **`/clud-review`** (if any source file in the diff, unless
   `--no-review` was passed). Address CRITICAL/HIGH findings before
   push; surface MEDIUM/LOW as advisory.
8. **Commit.** Conventional commit message. Reference the issue
   when one exists (`Closes #<num>` in the commit body so auto-close
   fires after the merge).
9. **Push.** Try direct push to the current branch
   (`git push origin <current-branch>`). On `protected branch` /
   `remote rejected` / `non-fast-forward` (for a protected branch),
   fall back to the PR path: `gh pr create --base <default-branch>`.
   First inspect repository policy: required checks, reviews and merge
   queues must be satisfied through the shared landing contract below.
   Only where policy permits immediate merging, use
   `gh pr merge <num> --admin --squash --delete-branch` without waiting
   for CI first; observe hosted validation in step 10. On a plain
   `non-fast-forward` / `fetch first` rejection of an unprotected
   branch, run the Push-Race Recovery procedure below instead.
10. **Observe delivery.** After a direct push or permitted immediate merge,
    start the exact commit's hosted run watch (`github/run_watch.py`) or
    `github/pr_merge_watch.py` in the background and wait for completion
    notification. Report failures and carry the fix forward in a new PR; do
    not stop at "pushed" or claim validation while checks remain pending.
    To wait use `pr_merge_watch`; for a one-off read ask only for unresolved
    items ([Reading CI status](https://github.com/zackees/clud/blob/main/docs/architecture/ci.md#reading-ci-status-without-flooding-context-2312)).
    Repository rules, required checks/reviews and merge queues override the
    immediate admin-merge shortcut above; never bypass protections. In those
    repositories use the [shared landing contract](https://github.com/talos-hq/clud/blob/main/docs/architecture/launch-plan.md#landing-preference).
11. **Retire the branch once its PR merged** (PR path only). In this
    checkout run `"$CLUD_EXE" gc branches --retire`. It switches back to
    the default branch, fast-forwards it and deletes the landed branch
    locally and on `origin`. A squash merge makes the branch look
    unmerged to git, so do not judge it by hand
    ([DD-230](https://github.com/talos-hq/clud/blob/main/docs/DESIGN_DECISIONS.md#dd-230-a-landed-branch-is-retired-from-the-primary-checkout-too-and-nudged-at-session-start)).
    If it warns that `delete_branch_on_merge` is off, pass that on to
    the user.
12. **Surface result.** One-line summary: commit SHA on the target
    branch, push mode (`direct` or `pr-admin-merged-<pr-num>`), and
    any non-default test/lint/review notes. A recovered push race is
    reported as a note (`push: direct after rebase onto <sha>`), not
    as a blocker.
13. **Release this run's checkout claim** with `clud claim release-own`.

## Push-Race Recovery

This skill owns this procedure. An unrelated commit landing on the
remote between your commit and your push is routine, not a blocker.
On a non-fast-forward rejection:

1. `git fetch origin <branch>` and inspect the incoming commits:
   `git log --oneline HEAD..origin/<branch>` and
   `git diff --name-only HEAD...origin/<branch>`.
2. Require a clean worktree (`git status --porcelain` empty). Any
   unexpected local change: stop and surface it; do not stash it.
3. `git rebase origin/<branch>` (normal rebase, no `--force` options).
   On any conflict: `git rebase --abort`, surface the conflicting
   files, and stop.
4. If the incoming commits touch any file your commit touches, re-run
   the cheap gates for that change class (lint + targeted test, or the
   `skills::` guardrails for SKILL.md). Otherwise skip re-verification.
5. Retry a normal `git push origin <branch>` exactly once. A second
   rejection: stop and surface it; do not loop.

Never force-push (`--force`, `--force-with-lease`, `+refspec`) at any
step.

For code changes specifically, the targeted-test step IS the
RED -> GREEN signal: identify or write the smallest test that
exercises the change (RED before the edit if one didn't exist),
make the change, then rerun that focused test until it passes
(GREEN). The full-suite version of RED -> GREEN belongs to a
`/goal`-driven run; this skill's targeted-only variant trades coverage
breadth for iteration speed.

## Failure Modes To Avoid

- **Editing files outside the user's stated scope.** Speed mode
  doesn't license drive-by refactoring.
- **Suppressing lint or test failures to "just push it."** That
  defeats the purpose of running them at all.
- **Calling a SKILL.md change "docs" and skipping the bundled-skill
  guardrails.** A SKILL.md edit IS a guardrail-tested change — run
  `soldr cargo test -p clud --lib skills::`.
- **Force-pushing the current branch to win a race with another
  contributor.** Recover a clean race with Push-Race Recovery; stop
  on a conflict or a second rejection.
- **Reporting a recovered push race as blocked.** If the rebase was
  clean and the retry pushed, the task is delivered.
- **Admin-merging a PR with failing local gates.** The PR-fallback
  path only fires when direct push is rejected for protection
  reasons; lint / test / review must already be green.
- **Mis-classifying a polyglot change.** A PR touching `.rs` and
  `.md` files is a source-code change for review purposes; run the
  source gates.
- **Falling back to the full test suite when no targeted test
  exists.** Print the documented notice and continue; if the change
  is risky enough that "no test" feels wrong, use `/goal`
  instead.

## When NOT to use

- **The change is large or risky.** Use `/goal` — lock in the
  deliverable and drive to it with full rigor (worktree,
  RED -> GREEN, full suite, CI).
- **The change touches code paths you don't fully understand.**
  Investigate first under `/goal`.
- **You're not the sole author of the current feature branch.** Use
  `/goal` and ship through a PR on a fresh disposable worktree so the
  work gets reviewed.
- **You're on a third-party repo where admin-merge isn't allowed.**
  Use `/goal` and let CI do its job; merge only after green.
- **You need to drive an existing open PR to merge.** Use `/goal`
  with the merge as the locked-in deliverable.

## Related

- `/goal` — locks in a deliverable and blocks stopping until it's
  met. The escalation path whenever this skill's speed tradeoffs are
  the wrong call.
- [[clud-review]] — invoked on source-code changes by Hard Rule 4
  and Workflow step 7.
- [[clud-git]] — worktree / branch / process-audit playbook when a
  push or teardown gets messy.

---
name: clud-git
description: Worktree, branch, process-audit, and quarantine playbook. Single source of truth reusable by /clud-fix-quick, /clud-issue-triage, and /goal runs. Use this when the user asks for worktree cleanup, stale-branch teardown, retiring a merged (including squash-merged) branch, or a process audit on a leftover dir.
triggers:
  - When the user asks to clean up worktrees, leftover .claude/worktrees/ directories, or stale feature branches
  - When a PR merged and its branch is still checked out, or the session context says the checkout is parked on a landed branch
  - When the user reports "shells appear stuck" or worktree removal failing with file-lock errors on Windows
  - When the user invokes "/clud-git"
  - When another skill needs the worktree-create + teardown + audit playbook (delegated from /clud-fix-quick, /clud-issue-triage, or a /goal run)
---
<!-- managed-by: clud -->

# /clud-git

Single source of truth for the worktree / branch / process-audit / quarantine playbook. `/clud-fix-quick`, `/clud-issue-triage`, a `/goal` run, and any future skill delegate here instead of copying the procedure as prose.

The launcher exports `CLUD_EXE` as its own absolute path. Examples below use Bash quoting; in PowerShell use `& $env:CLUD_EXE`, and in cmd use `"%CLUD_EXE%"`. Outside a clud-launched session, set `CLUD_EXE` to the intended binary first.

## Do you need a worktree at all?

Default: **no**. Work in the current checkout, on a branch: `git fetch origin && git switch -c <branch> origin/main` when the tree is clean. A fresh worktree starts with a cold Cargo `target/`: on clud, 46–72 s and 889 MB for a `check` alone, more for a test build (#1685, `docs/architecture/skill-system.md`). Read-only work (triage, review, investigation) reads at a revision with `git show <rev>:<path>`, `git grep <pattern> <rev>` and `gh`, and never needs a checkout.

Create a worktree only under one of these conditions, and name the condition when you do (#1971):

1. **Concurrent writers must build at the same time**, such as `/grind` parallel mode with each goal on its own branch. For Rust repos, prefer sequential mode and state the cost.
2. **The user explicitly asks** for an isolated checkout.
3. **The main checkout holds someone else's in-progress work** (another session's branch, or a dirty tree you may not stash), and the task must build or test.

The three hard rules below are non-negotiable — this skill exists to centralize them, not to relax them.

## Three hard rules

1. **Worktrees live in clud's worktree root.** Create every worktree at `~/.clud/worktrees/<repo>-wt-<name>/`, where the daemon reclaims it once the work lands. `<name>` is the issue number, or `PR<num>` for a pull request: `clud-wt-1962`, `clud-wt-PR1234`. If you pick another spot, the refusal names the exact command to run instead. clud's command hook refuses `git worktree add` anywhere else (the only other allowed place is `/grind`'s own `<repo>/.clud/grind/worktrees/`), however the command is spelled (#1962). A worktree inside the repo is never reclaimed and gets walked by linters and builds.
2. **Process audit before destructive removal.** Windows file locks routinely cause `git worktree remove` to fail with `Permission denied` / `Access is denied`. Always audit which processes hold the directory before resorting to `--force`, then prefer `"$CLUD_EXE" trash` over `rm -rf` retry loops.
3. **Never blind-loop `rm -rf`.** A `rm -rf` retry loop hides the actual problem (a process holding a file open) instead of fixing it. Audit, stop the specific offender, OR quarantine via `"$CLUD_EXE" trash`.

## Worktree creation playbook

Only for one of the allowed conditions above; otherwise branch in place.

1. **Sync main.** `git fetch origin main` so the new worktree starts from current main, not stale local state.
2. **Stale worktree prune.** `git worktree prune` to clear git's view of any worktrees deleted on disk without proper removal.
3. **Create in the worktree root.** `git worktree add -b <branch> ~/.clud/worktrees/<repo>-wt-<name> origin/main` (see hard rule 1). `<repo>` is the repository's directory name and `<name>` is freely chosen — convention is `<slice-or-feature>-<issue-number>` so leftover dirs are searchable.
4. **RED -> GREEN inside the worktree.** All edits, lint, tests, and commits happen inside the worktree path. Never edit the main checkout for worktree-scoped work.

### Rust target directory for sequential work

For one Rust build or test at a time, you may opt in to reusing the main
checkout's Cargo target directory: set `CARGO_TARGET_DIR=<main-checkout>/target`
for each worktree command. Keep the target in the persistent main checkout, not
in a worktree that will be removed. Cargo serializes builds sharing a target,
and dependencies can be reused. Workspace crate outputs and
`target/debug/clud` can be replaced by a build from another branch, so never
share the target while different worktrees build or test concurrently. In
particular, integration tests that locate a binary through `CARGO_TARGET_DIR`
must use a private target if another branch can build at the same time.

This is opt-in: leave each worktree's target private when concurrent builds or
tests are expected. Prefer the local checkout for sequential Rust work when it
already has a warm target. The measurement and limits are in
`docs/architecture/skill-system.md` (#1685).

## Worktree teardown playbook

1. **`git status` clean check.** Inside the worktree, confirm no uncommitted changes — refuse to tear down a dirty worktree without explicit user OK.
2. **Process audit.** Before removal, identify what holds the worktree path open:
   - **Windows**: `Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -like "*<branch>*" -or $_.CommandLine -like "*<worktree-path>*" }`. Common holders: `zccache.exe`, `rust-analyzer.exe`, the agent's own PowerShell session (cwd anchored inside the dir).
   - **Unix**: `lsof +D <worktree-path>` or `fuser -m <worktree-path>`.
3. **Stop only specific abandoned holders.** Never kill unrelated processes. If the holder is the agent's own shell session (cwd anchored), cd to an absolute path outside the worktree first.
4. **`git worktree remove --force`.** Pass `--force` only after the audit shows no useful holders. If it still fails with "Access is denied" / "device or resource busy", fall through to step 5.
5. **`"$CLUD_EXE" trash` quarantine fallback.** `"$CLUD_EXE" trash <worktree-path>` moves the path to `~/.clud/trash/<timestamp>/` for the daemon's GC sweeper to reap when handles release. This is the documented fallback — see [[clud-windows-trash]] for the file-lock case.
6. **`git worktree prune`** + verify `git worktree list` no longer mentions the path.
7. **Delete the local branch** if it was created for the worktree: `git branch -D <branch>` for already-merged work; `git branch -d` for non-merged (which will refuse and force you to confirm). Never delete a `grind/*` branch (local or remote) that has an open PR (`gh pr list --head <branch> --state open`): it is a `/grind` feature branch holding landed goals until its feature PR merges. Push a worktree's branch before removing the worktree; a merged PR's commits stay reachable at `refs/pull/<n>/head`, unpushed commits do not.

## Branch retirement playbook (DD-230)

A merged PR's branch must not stay checked out or pile up. Squash merges
rewrite the commit, so `git branch --merged`, `git cherry` ahead counts and
"ahead 1" upstream status all report a merged branch as unmerged. Do not
judge landed-ness by hand; use the squash-aware report:

1. **Report.** `"$CLUD_EXE" gc branches` fetches with `--prune`, then marks
   each local branch `retire` (merged PR head covers the tip, patch match, or
   no commits of its own), `follow-up` (commits after its merged PR: unmerged
   work), or `keep` (open PR, no PR, uncommitted changes, a worktree, doubt).
2. **Retire — this is the action, not a bare `git switch`.** Whenever the report shows a `retire` row, run `--retire`: a manual `git switch <default>` leaves every landed branch behind, which is the pile-up this playbook exists to stop. Retiring is safe by construction (each branch is re-verified at its tip just before deletion; `origin` deletes are leased). `"$CLUD_EXE" gc branches --retire` switches a landed current
   branch back to the default branch, fast-forwards it, and deletes every
   landed branch locally and on `origin` (leased; never a branch that is the
   base of an open PR). `--keep-remote` keeps `origin` copies;
   `--sweep-remote` also clears `origin` branches whose merged PR head is
   their exact tip.
3. **Follow-up rows are work, not clutter.** Open a new PR for them, or tell
   the user why not. Never delete one.
4. **Repository setting.** When the report warns that
   `delete_branch_on_merge` is off, tell the user. GitHub then never deletes
   merged head branches. Enabling it (`gh api -X PATCH repos/{owner}/{repo}
   -F delete_branch_on_merge=true`) needs repo admin and is the user's call;
   do not change it unasked.

When to run it: right after a PR you own reports `MERGED` (the
`pr_merge_watch.py` `RETIRE-LOCAL` line), when the `SessionStart` context
says the checkout is parked on a landed branch, and before starting new
work on a non-default branch.

## Process-audit playbook (standalone)

When the user reports "my shell appears stuck" or `git` commands hang on a Windows path:

1. **Identify the anchored cwd.** A Bash or PowerShell session whose cwd is inside a deleted-or-moved dir will fail every command because the hook system can't resolve its own scripts. Look for the error pattern `python .claude/hooks/check-soldr.py: No such file or directory`.
2. **Cd to a known-good absolute path** before running any other command: `cd "C:/Users/niteris/dev/clud2"` (or your repo root).
3. **Re-audit for stale holders** of the original problem path. The shell session unanchoring usually clears the immediate issue; lingering processes like zccache or rust-analyzer may still need explicit attention.

## Standard recovery patterns

| Symptom | Fix |
|---|---|
| `git worktree remove` → "Access is denied" | Process audit → stop specific holders → retry. If still failing → `"$CLUD_EXE" trash`. |
| 10+ leftover dirs under `.claude/worktrees/` (made before #1962) or `~/.clud/worktrees/` | Batch `git worktree remove --force` for each git-tracked one, then `"$CLUD_EXE" trash` for the orphan dirs. Delete the merged branches with `git branch -D` afterward. |
| "Shells appear stuck" after a teardown | Bash cwd is anchored on a deleted dir. Cd to absolute path. PowerShell tracks cwd independently and is usually the safer escape. |
| Local branch list cluttered with old feature branches | `"$CLUD_EXE" gc branches` (report), then `"$CLUD_EXE" gc branches --retire`. Never use `git branch --merged` here: a squash merge leaves every merged branch looking unmerged to git. |

## Delegation contract

When called from `/clud-fix-quick`, `/clud-issue-triage`, or any other skill:

- The caller provides the `<branch>` / `<short-name>` and the operation (create / teardown / audit).
- This skill executes the playbook and returns structured evidence: what was created, what was torn down, what was quarantined, what processes were stopped.
- This skill does NOT manage `/goal` lifecycle — the caller owns its own goal hook.
- This skill does NOT push or merge PRs — those belong to the caller.

## Hard rules summary

0. **No worktree without a named allowed condition**; branch in place by default.
1. **Create worktrees only under `~/.clud/worktrees/`.**
2. **Process audit before destructive removal.**
3. **`"$CLUD_EXE" trash` is the documented Windows fallback, not `rm -rf` retry loops.**
4. **RED -> GREEN** for any code edits made inside a worktree.
5. **Never kill unrelated processes** during a stale-holder cleanup.
6. **Cd to absolute path** before running git commands when shell behavior gets weird.
7. **Retire merged branches with `"$CLUD_EXE" gc branches --retire`**, never with `git branch --merged`, so the checkout returns to the default branch and the branch list doesn't accumulate.

## When NOT to use this

- The caller wants to ship a PR, or drive an existing open PR to merge — that's a `/goal` run (which delegates here for the worktree mechanics).
- The caller wants to investigate or design — this skill is mechanical; design discussion is the parent skill's responsibility.

## Related

- `"$CLUD_EXE" trash` subcommand — quarantines paths under `~/.clud/trash/<timestamp>/` for daemon GC.
- `/clud-fix-quick` — speed-mode skill that wants the same worktree teardown without the full PR ceremony.

## Landing a working branch

When code work is ready, follow the [shared landing contract](https://github.com/talos-hq/clud/blob/main/docs/architecture/launch-plan.md#landing-preference) before tearing down
its branch or checkout. Preserve work when checks, review or permissions block
landing and report the remaining gate.

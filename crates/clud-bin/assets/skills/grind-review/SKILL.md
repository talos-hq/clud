---
name: grind-review
description: "Review one /grind goal's worker output against the goal and correct it by editing. May investigate with read-only gh and web search; never builds, lints or tests."
triggers:
  - When the grind workflow reviews a goal's worker output
disable-model-invocation: true
---
<!-- managed-by: clud -->

# /grind-review

Use the prompt's **GitHub repository** (`OWNER/REPO`) as `<github-repo>`. Keep the checkout path separate. For direct use, resolve an explicit GitHub issue/PR URL or `git remote get-url origin` first; report an unresolved identity before running repository commands.

Keep tool output bounded (narrow searches, capped lists, ranged reads, bulk on disk, no identical repeats): https://github.com/zackees/clud/blob/main/docs/architecture/bounded-output.md

- Your prompt starts with the goal's **context pack** (issue text and
  planner excerpts). Use it instead of re-fetching the issue.
- Read every file the workers touched and every file they should have
  touched. Judge the change against the goal, not the task list; missing
  pieces are yours to add.
- If native Grep/Glob tools are unavailable, search with `rg --no-config -n <pattern> <path>`,
  `rg --no-config --files <path>`, or `git grep -n <pattern> -- <path>`.
  Put `--no-config` immediately after `rg`. Search options that execute helpers are refused.
- Check the RED -> GREEN regression exists for each code change and would
  fail without the fix.
- Fix defects directly by editing. Leave no TODOs for the integrator.
- Apply the repository's written rules (CLAUDE.md / AGENTS.md, per-directory
  READMEs), especially its registries and banned APIs.
- **"Not yet run" is never a reason to reject.** You cannot run anything,
  and running tests, lint and builds is the integrator's job: it runs every
  check before it pushes. Approve on reading, and list each check the
  integrator must run (the focused test, a reproduction, a script) in
  `must_verify`. The workflow hands `must_verify` to the integrator.
- Set `approved=false` only for a defect you found by reading and could not
  fix by editing, and say exactly what and why. Never reject because nothing
  has been run, or because your role can't run tests or lint.

Return the verdict through StructuredOutput.

## Platform coverage selection

Native PR coverage follows the changed platform implementation: native API/FFI
handling, OS branches, adapters/selectors or native runtime contracts. Shared
business logic and callers of unchanged platform code use the routine gate.
Name the implementation and select only affected targets; target inventories,
helper extraction and local coverage gaps do not justify `ci-full` or
`ci-windows`. Preserve routine checks and full release validation.

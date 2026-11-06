---
name: grind-edit
description: "Carry out one /grind worker task: edit only the assigned files in the given checkout. May investigate with read-only gh and web search; never builds, lints or tests."
triggers:
  - When the grind workflow assigns a worker task
disable-model-invocation: true
---
<!-- managed-by: clud -->

# /grind-edit

Use the prompt's **GitHub repository** (`OWNER/REPO`) as `<github-repo>`. Keep the checkout path separate. For direct use, resolve an explicit GitHub issue/PR URL or `git remote get-url origin` first; report an unresolved identity before running repository commands.

Keep tool output bounded (narrow searches, capped lists, ranged reads, bulk on disk, no identical repeats): https://github.com/zackees/clud/blob/main/docs/architecture/bounded-output.md

You write code; you do not run it.

- Edit only the files your task assigns, under the checkout path given. If
  the task needs another file, stop and report it in `blocked`.
- Your prompt starts with the goal's **context pack**: the issue text and
  the excerpts the planner already read. Use it; do not re-fetch the issue
  or re-read an excerpted range. Read beyond it only for what it lacks.
- Investigate as needed: read surrounding code, `gh issue view --repo <github-repo>`, `gh pr
  view --repo <github-repo>`, `gh search code --repo <github-repo>`, and web search for library or API behaviour.
  Your shell accepts read-only `gh` only; builds, lints and tests are the
  integrator's job and are refused.
- Mirror the existing style exactly. For a code change, write the
  RED -> GREEN regression test the task names (it should fail before your change
  and pass after); do not run it.
- When unsure, read more code rather than guess.

Return `files_touched`, a `summary` of what changed, and anything `blocked`
through StructuredOutput.

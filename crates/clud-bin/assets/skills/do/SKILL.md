---
name: do
description: "Implement one issue URL or free-form goal all the way to merged PRs, starting from the right branch: RED -> GREEN, review, test, push, watch CI to green, merge, clean checkout. `clud do` seeds /goal with this same rendered contract."
triggers:
  - When the user explicitly runs /do or clud do
disable-model-invocation: true
allowed-tools: Bash(clud do-prompt:*)
---
<!-- managed-by: clud -->

!`clud do-prompt "$ARGUMENTS"`

If the line above still shows a command instead of a rendered prompt (the
harness did not run it, or `clud` is missing), run `clud do-prompt <target>`
yourself and follow its output. If that fails too, stop and tell the user;
do not guess the contract. Every code change keeps a RED -> GREEN focused
regression.


The rendered Starting point includes the target issue preflight. Resolve
`unknown` manually and stop on `blocked` or `needs-ready`. Follow the repo's
claim protocol before the first code change; re-check the issue before
`gh pr create` and before enabling auto-merge or queueing. If it was closed
(including as a duplicate) or claimed by someone else, stop and report why.

Follow the [shared landing contract](https://github.com/talos-hq/clud/blob/main/docs/architecture/launch-plan.md#landing-preference) when delivering changes. Preserve this role's scope; required checks, review, merge queues, `--safe` and explicit user
instructions take precedence over any immediate admin-merge shortcut.

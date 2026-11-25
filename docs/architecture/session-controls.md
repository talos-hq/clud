# Session controls (#1984)

A session control is a switch only the user can turn, enforced against every
agent in the session, subagents included. The first one gates git worktree
creation: refused unless the user opted in. Rationale:
[DD-176](../DESIGN_DECISIONS.md#dd-176-a-session-control-takes-consent-only-from-the-users-own-channels).

## How state moves

```
clud (foreground)                       sets CLUD_* in its own env (main.rs), e.g.
 │                                      CLUD_ALLOW_WORKTREES=1 for `--worktree`
 │  runner::child_env → claude's env    every CLUD_* passes through unchanged
 │  claude --settings <launch tmp>      clud_hooks_compile::deletion_safety_fragment
 ▼                                      registers PreToolUse + UserPromptSubmit
claude
 ├─ user prompt ──► clud-cmd-scan --event UserPromptSubmit
 │                   stdin {session_id, prompt}  → worktree_consent::handle_prompt
 │                   says yes? → daemon::session_flag_set(S, "worktrees", true)
 │                   exit 0, no stdout (stdout joins the conversation; exit 2 erases the prompt)
 ├─ any agent's tool call ──► clud-cmd-scan  (PreToolUse, matcher = GUARDED_TOOLS)
 │                   stdin {session_id: S, agent_id?, tool_name, tool_input}
 │                   worktree_consent::refusal(S) → deny_json, exit 2
 └─ Claude Code worktree ──► clud-cmd-scan --event WorktreeCreate
                     claude_worktree_hook::run_create → refusal(S) → exit 1
```

- **Identity is the payload `session_id`.** A subagent's hook payload carries
  its parent session's id (and its own `agent_id`), so one flag covers the
  whole tree. `/clear` starts a new id, so controls reset. Do not key on
  `CLUD_CHECKOUT_SESSION_ID`: it is unset in several launch modes and a nested
  launch can inherit a stale one.
- **The environment is inherited, not writable by the agent.** Every hook is
  a child of `claude`, which is a child of `clud`, so a hook reads clud's
  launch environment. `VAR=1 cmd` inside an agent's Bash command changes only
  that command's environment. The same property makes `CLUD_BAD_CMD_OVERRIDE`
  read the real environment (`classify_override`).
- **The daemon dictionary** (`daemon/session_flags.rs`) is
  `session_id → key → JSON value`, in memory. Requests share the checkout
  claims' TCP line lane, tagged `session_flag_op`. The client connects only;
  it never starts a daemon, so a hook cannot stall on startup. A restart
  forgets every flag.

## The worktree control

Gated only in sessions clud launched with its prompt hook: clud sets
`CLUD_WORKTREE_GATE=1` for a Claude launch that is not unsafe (and clears an
inherited one otherwise). A plain `claude` or Codex session running
`clud-cmd-scan` from a committed or global hook config cannot record consent,
so it is not gated. In a gated session, a worktree is allowed when any of
these holds, else refused with a message telling the agent to have the user
type "use worktrees" (a yes to the agent's question does not count):

1. the daemon flag `worktrees` is `true` for the session (the prompt said yes:
   "use worktrees", "you can use a git worktree", "worktrees ok", or an
   explicit direction such as "do this in a worktree"; a no such as
   "no worktrees" or "don't use the work tree" never counts, and "branch"
   alone says nothing);
2. `CLUD_ALLOW_WORKTREES=1` in the launch environment, which `clud --worktree`
   or `-w` also sets;
3. a `/grind` run is active for the session (`grind_facts`), whose caps
   already govern its worktrees;
4. unsafe mode.

It is checked for Bash `git worktree add` (after the parse in
`block_bad_cmd`, once the [location guards](shell-ast.md#worktree-location-guards)
pass), `EnterWorktree`, `Agent`/`Task` with `isolation: "worktree"`, and in
the `WorktreeCreate` handler, which every Claude Code worktree goes through.
A session the daemon cannot answer for is refused. The rule has no
`CLUD_BAD_CMD_OVERRIDE` id.

**Limits.** This deters a forgetful agent; it is not a security boundary
(DD-176). Codex launches are not gated. The Bash check sees what the parser captures; a git alias
(`git wt add`) is not a `worktree add`. A pasted document that says "use a
worktree" counts as the user's yes.

How to add another control: the `session-control` skill
(`.claude/skills/session-control/SKILL.md`).

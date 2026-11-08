# `git` / `gh` telemetry pass-through (#1486)

Inside a clud session, `git` and `gh` on PATH are aliases of the one `clud`
binary that run the real tool unchanged and record one telemetry line per
invocation. They exist to gather telemetry on how agents use git and gh.
**Nothing is ever refused**: every argv, including `git clone` and
`git worktree add`, reaches the real binary with the same argv, stdin, stdout,
stderr and exit status. (#1486 first proposed refusing clones. The user
narrowed it to telemetry only: "we just want to essentially use this for
telemetry gathering".)

Alias mechanics are owned by [shim-dispatch.md](shim-dispatch.md): the
registry rows, the session ABI stamp, and the fail-open passthrough outside a
session. `gh pr checks --watch` still gets its upgrade
([gh-watch-shim.md](gh-watch-shim.md)), and that is recorded too.

## The names and the real binaries

`git` and `gh` (`.exe` on Windows) are `Passthrough` rows in
`shim_registry::SHIMS`, installed in `~/.clud/state/rm-shim`
([DD-121](../DESIGN_DECISIONS.md#dd-121-helper-executables-are-argv0-aliases-of-the-one-clud-binary)).
`shim_session::activate_rm` resolves the real `git` and `gh` from the PATH
the session had *before* the alias directory was prepended. It exports them
as `CLUD_GIT_SHIM_TARGET` / `CLUD_GH_SHIM_TARGET`, skipping every alias
directory and every copy of `clud`, so an alias can never run itself.
Dispatch revalidates the target on each call. A missing or invalid target, or
no session at all, makes the alias a plain passthrough to the next real
binary on PATH, with no telemetry.

clud's own spawns (GC probes and reclaim, `worktrees.rs`, grind) bypass the
alias. `subprocess::command_spec_for_subprocess` rewrites a bare `git` / `gh`
to `shim_registry::real_program`, the session target when one is valid.

## Running the child

In a session the alias spawns the real binary as a child with inherited
stdio, waits, records, and returns the child's exit code (`run_child` in
`shim_main.rs`). It does not `exec`, because an `exec`ed process cannot record
anything afterwards
([DD-131](../DESIGN_DECISIONS.md#dd-131-the-git--gh-telemetry-shim-waits-for-the-child-instead-of-execing)).
On Unix it behaves like a shell running one command. While the child runs
the shim ignores SIGINT/SIGQUIT: Ctrl+C reaches the child through the
terminal's process group. It forwards SIGTERM/SIGHUP sent to the shim alone.
If the child dies by signal N, the shim records `128 + N` and then re-raises
N on itself, so its caller sees the same wait status as before. On Windows it
waits and returns the code, which matches what the shim already did there.

## The record

`crates/clud-bin/src/shim_telemetry.rs` appends one JSON line per invocation
to `<state>/logs/shim/git-gh.jsonl`. `<state>` is `~/.clud/state`, or
`$CLUD_DAEMON_STATE_DIR` when set.

| field | value |
| --- | --- |
| `ts_ms` | start time, Unix ms |
| `tool` | `git` or `gh` |
| `argv` | arguments after argv\[0\] (lossy UTF-8, credential shapes redacted before writing) |
| `cwd` | working directory |
| `exit_code` | the child's code, `128 + N` for signal N, `126` if it could not start |
| `duration_ms` | wall time of the invocation |
| `session_id` | `CLUD_SESSION_ID` (or the grind session id), else `null` |
| `pid` | the shim's pid |
| `parent` | parent process name from `/proc/<ppid>/comm` on Linux, else `null` |

Telemetry never changes behaviour. Every I/O error is ignored, the streams
are never touched, and there is no network and no extra process. No
environment value is logged. The daemon's HTTP telemetry sink was not used,
because it is a network POST. `~/.clud/state` has no sweeper (#1014), so the
file rotates to `git-gh.jsonl.1` at 8 MiB (`MAX_BYTES`), keeping at most two
files.

## Tests

- `shim_telemetry.rs`: record fields and no env, unwritable path ignored,
  rotation.
- `shim_registry.rs` / `shim_session.rs`: `git` and `gh` are aliases, and
  neither a target nor `real_program` ever resolves into an alias directory.
- `tests/test_git_shim.py`: argv, output and exit code pass through for git
  and gh, including `clone`, `worktree add` and `repo clone`. It also checks
  the telemetry line's fields, an unwritable state dir, death by signal, a
  self-pointing target not recursing, the outside-session passthrough with
  `git: command not found`, and the Windows `git.exe` alias.

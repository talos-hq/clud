# `clud-shim` dispatch: one registry, one entry path, fail open

`clud-shim` is the personality of the one `clud` binary behind every PATH alias
clud installs: `python` and `python3` in `~/.clud/state/shims`, and `rm`, `gh`, `git`, <!-- python-name-lint: allow -->
`safe-rm`, `safe-mktemp` in `~/.clud/state/rm-shim`. Each alias is a hardlink, symlink or copy of `clud`
(hardlink first; [DD-121](../DESIGN_DECISIONS.md#dd-121-helper-executables-are-argv0-aliases-of-the-one-clud-binary);
since #2558 a symlink into the versioned [toolstore](toolstore.md))
that `multicall::maybe_run` routes here by argv\[0\] before any other startup
work. The code is `shim_main.rs` and `shim_main/dispatch.rs`. This doc owns the contract every alias shares (#1546). What an
alias does *inside* a session is owned elsewhere:
[rm-protection.md](rm-protection.md) for `rm`, [rm-tools.md](rm-tools.md) for
`safe-rm` and `safe-mktemp`, [gh-watch-shim.md](gh-watch-shim.md) for `gh`, and
[git-gh-telemetry-shim.md](git-gh-telemetry-shim.md) for the `git` / `gh`
telemetry pass-through.

## The registry

`crates/clud-bin/src/shim_registry.rs` holds `SHIMS`, one `ShimSpec` row per
alias name: its `ShimKind`, the session keys it reads, its `Fallback`
(`Passthrough` or `Native`), and the alias directories it is installed into.
Everything else derives from it:

| Consumer | Derived from the registry |
| --- | --- |
| `shim_main/dispatch.rs` | name lookup, session keys, passthrough |
| `shim_install::alias_names` / `rm_alias_names` | the files installed per directory |
| `shim_session::activate_rm` | the alias dir, the ABI stamp, the `gh` target |
| `shell/nounset.rs` | the key the BASH_ENV re-prepend reads |
| `runner::child_env_policy_keys` | the session keys the policy owns |

Adding a shim is one row plus one handler arm. `ShimKind` is matched
exhaustively in dispatch, so the compiler refuses a row without a handler.
`clud-shim --registry` prints the table as JSON; the Python tests iterate it.

## The single entry path

`main` calls `dispatch::run`, and nothing else in the binary decides an exit.
Dispatch owns three things:

1. **Session detection.** A session is valid when `CLUD_SHIM_ABI` equals the
   binary's `SHIM_ABI` and the alias's own keys are present and sane:
   `CLUD_PYTHON_SHIM_TARGET` for `python`, `CLUD_GH_SHIM_TARGET` for `gh`,
   `CLUD_GIT_SHIM_TARGET` for `git`, an existing absolute `CLUD_RM_SHIM_DIR`
   for `rm`.
2. **Target resolution.** A target must be absolute and executable, outside
   every shim directory, and not a copy of the running shim.
3. **Fail-open passthrough.** Without a valid session a `Passthrough` alias
   execs the next same-named executable on PATH after its own directory. It
   skips every shim directory and every byte-identical copy of itself, so a
   chain of aliases only moves later on PATH and cannot loop. Argv, stdio and
   the exit status are the real binary's. Passthrough prints nothing; the
   only failure is `127` with one `<name>: command not found` line when no
   real binary exists.

A `Native` alias (`safe-rm`, `safe-mktemp`) is a clud command, not a relay. It
runs its own out-of-session mode: `safe-rm`'s roots fall back to the git
checkout or the cwd, and `safe-mktemp` without a session id creates nothing.

Handlers receive a validated `Session` and never read a session key. The
`session_contract_is_owned_by_dispatch` test in `shim_main.rs` fails if a
handler names a session key or `CLUD_EXE`, calls `exit`, returns `127`, or
re-grows an argv\[0\] chain. It also fails if dispatch or the installers
hardcode an alias name. `shim_registry`'s own tests keep the key literals in
that one file and require every deletion-rules safe alias to have a row.

## Version skew

The alias directories are per user, so every live session of every
installed clud version shares them. A newer clud installs aliases that an
older session's env has no keys for. #1546 was this case. A 2.8.14 session
had `rm-shim` on PATH but no `CLUD_GH_SHIM_*` keys. After 2.8.20 added the
`gh` alias to that directory, the alias exited 127 and broke the session's
`gh auth git-credential` helper.

The ABI stamp makes that meeting fail open. An env built by another clud
version has no `CLUD_SHIM_ABI` or a different value, so every alias in it
passes through to the real binary. Bump `SHIM_ABI` only when a key changes
meaning. A new alias or key needs no bump, because an older session simply
lacks the key.

## Running sessions after an upgrade

A session keeps the alias directory it launched with, so after an upgrade
its aliases would run the old binary until some later launch relinked them.
`main` calls `shim_install::refresh_running_session_aliases` on every `clud`
CLI start, before clap. The session's statusline (every 2 s in Claude Code),
session hooks and `clud tool` calls run the installed `clud` by absolute
path, so the first of them after an upgrade relinks `rm-shim` to the new
binary. It relinks only when all of these hold:

- the running `clud` is the session's own `CLUD_EXE`;
- the session's `CLUD_SHIM_ABI` is the binary's `SHIM_ABI`, so the relinked
  aliases still validate this session (an ABI bump waits for a launch);
- the session's `CLUD_RM_SHIM_DIR` is the shared `~/.clud/state/rm-shim`,
  and it exists (nothing is created);
- some alias is stale, and none is as new as or newer (mtime) than the
  running `clud`, so two installs never take the directory back and forth.
  The check is repeated under the install lock.

The relink is the launch installer's: under `.rm-shim.lock`, each alias is
staged and renamed over the old one, never truncated, so a running alias
keeps its old inode and the next call gets the new one. It tries hardlink,
then symlink, never a copy, and any failure is silent and retried on the
next call. Where neither can be made (a home on another volume than `clud`
and no symlink privilege on Windows), that retry fails every time and the
session waits for its next launch, as before. The session's env does not change: a behavior that needs a new
key reads a setting when the key is absent, as the `gh` read broker does
([gh-read-broker.md](gh-read-broker.md#setting)). Why the aliases are
relinked rather than delegating to `$CLUD_EXE`:
[DD-152](../DESIGN_DECISIONS.md#dd-152-a-running-sessions-aliases-are-relinked-by-the-installed-clud-not-self-delegated).

## Defense in depth for git credentials

`activate_rm` sets `GIT_TERMINAL_PROMPT=0` and, unless the caller chose
one, an empty `GIT_ASKPASS`. An agent cannot answer a prompt, so a failed
credential helper now errors instead of opening the desktop's `SSH_ASKPASS`
dialog. `SSH_ASKPASS` itself is untouched, so ssh can still unlock keys. An
explicit `GIT_ASKPASS`, such as an editor's, is preserved.

## Out-of-session `rm`

The catastrophe floor is session-only (owner decision on #1546). Outside a
valid session `rm` is the next real `rm`, with no floor, no guard flags and
no audit record, like every other passthrough alias. Tests that exercise
the floor stamp a session first (`tests/shim_env.py`).

# Session `gh` watch shim

The child environment installs `gh` beside the active `rm` and `safe-rm`
aliases in `~/.clud/state/rm-shim`. `shim_session::activate_rm` resolves the
real executable from the incoming PATH *before* prepending that directory,
then exports its absolute path as `CLUD_GH_SHIM_TARGET`. Foreground and daemon
launches share this path through `runner::apply_child_env_policy`.

`clud` dispatches (as `clud-shim`) by invoked filename through its single dispatch path;
without a valid session the alias is the next real `gh` on PATH (see
[shim-dispatch.md](shim-dispatch.md)). In a session, for ordinary commands it
runs the real executable as a child with the original arguments and inherited
stdio, and returns its exit status. Signals are handled shell-style so the
wait status is unchanged ([DD-131](../DESIGN_DECISIONS.md#dd-131-the-git--gh-telemetry-shim-waits-for-the-child-instead-of-execing)). A PR-check watch is
instead parsed against a deliberately narrow flag contract. The selector
(a number, a branch, a PR URL, or none) goes to the bundled watcher as is.
The watcher resolves it on `--repo`, else on the `origin` remote, and a URL
names its own repo. The shim does no lookup of its own: a bare
`gh pr view <branch>` in a fork resolves to the parent repository (#1741).
The watcher runs through
the pinned `CLUD_EXE tool run github/pr_merge_watch.py` path; without a
valid `CLUD_EXE` the watch runs on the real executable unchanged. The
upgrade is fail-fast only: it passes `--cancel-on fail`, so review activity
or a closed PR ends the watch without cancelling anything, and a failure
cancels only the failing run and older runs of its workflow, judged by the
watcher's supersession rule (#1742). The failing run is spared while any of
its jobs is still in progress, so sibling jobs keep their results
(obs-rust/obs-studio#13). Unsupported
flags fail explicitly so the shim cannot silently change a future CLI's
output or wait contract.

The command guard permits the canonical PR-check watch only when
`CLUD_GH_SHIM_ACTIVE=1`. It still denies PR polling loops. Run watches
(`gh run watch <id>`) are not PR waits and fall through
([DD-189](../DESIGN_DECISIONS.md#dd-189-only-pr-waits-are-routed-through-the-fail-fast-watcher)).
If installation or target resolution fails, that marker is absent/zero and
the old guard denial remains; the alias itself still relays to the real `gh`.
Every in-session `gh` call, relayed or watched, is run as a child and
recorded; see [git-gh-telemetry-shim.md](git-gh-telemetry-shim.md). The `git.pr_wait_fail_fast` setting gates
the guard; disabling it leaves the alias as an ordinary relay for commands
that are not intercepted.

Ordinary commands are relayed with one exception. When `git.gh_read_broker`
is on (`CLUD_GH_READ_BROKER=1`), a `gh api` GET may be answered by the
daemon's read broker, also when `gh.backend = "ghd"`. The real `gh` still formats the brokered body, and
any miss relays the call unchanged. After a call that may write, the alias
also asks the broker to revalidate its cache; see
[gh-read-broker.md](gh-read-broker.md).

## `gh run watch`

`gh run watch <id>` re-reads the run over REST every 3 seconds (`-i`) for
as long as the run lasts, with no ETag. A few agents watching runs this way
drained the shared 5,000/h core limit. The alias therefore hands it to
`CLUD_EXE tool run github/run_watch.py <id> [--repo R] [--interval N]`.
The bundled watcher polls at least 30 s apart, revalidates with ETags, and
exits 124 on a resumable timeout. This upgrade does not depend on
`git.pr_wait_fail_fast`, because it exists for the budget. `--exit-status`
is implied (the watcher exits 1 when the run fails) and `--compact` is
dropped. A run watch the watcher cannot take is refused with exit 2 and a
message naming `github/run_watch.py` and `github/pr_merge_watch.py`. That
covers no run id (gh would prompt), an unknown flag, non-UTF-8 arguments,
and a session without `CLUD_EXE`. It is never relayed to the real `gh`
([DD-222](../DESIGN_DECISIONS.md#dd-222-the-read-broker-sits-in-front-of-the-ghd-backend-and-gh-run-watch-is-upgraded)).
`gh_shim::run_watch_argv` is the translation. `shim_main/gh_relay_tests.rs`
and `tests/test_gh_shim.py` pin it.

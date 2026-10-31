# GC & Registry

`clud` maintains two separate `redb` databases for two separate concerns: a per-launch
**session cap** that bounds how many sibling `clud` processes may run concurrently, and a
**tracked-entry GC** that owns the lifecycle of `.claude/worktrees/agent-*`, `.extern-repos/*`,
and known sibling temp-clone directories across repos and across invocations. The two stores live
in different files, on different code paths,
with different ownership models — they have nothing to do with each other besides both being
`redb`-backed, and confusing them is the single fastest way to wedge the system. This doc covers
both, plus the worktree scanner that feeds the GC store and the unrelated `--clean-worktrees`
subcommand.


## Where extern checkouts live

Since #986 a repo's foreign checkouts live **beside** it — `~/dev/myrepo` keeps
them in `~/dev/myrepo-extern/` — rather than in an in-tree `.extern-repos/`.
The GC watches both: a fourth `EXTERN_REPO_KIND` watch root points at the
sibling, alongside the legacy in-tree one, so checkouts users already have keep
being tracked while they move.

The sibling needed its own watch root rather than a widening of the
sibling-clone one, because that scanner inserts immediate children of the
repo's *parent* directory, and a checkout at `<repo>-extern/dep` is a
grandchild of it. Registry rows were already keyed on absolute paths, so
nothing about tracking, sweeping, or reclaiming changed.

`extern_root.rs` owns the location; see DD-053 for why it moved.

## Source reclamation safety and the #1790 incident

On 2026-10-04 the entire `ci.yml-extern` parent disappeared during active
cross-repository work. The deletion actor remains **unproven**. The 2026-10-20
investigation checked the retained audit log, the 2.8.27 scanner naming rules,
tracked-entry deletion, worktree fallback removal, and the build-target sweep.
The retained audit contained no exact parent deletion record; it did contain
child deletions from `gc.entry-delete` and `worktree.fallback-remove`. This
negative result does not exclude an unaudited operation or an external actor.
The scanner's sibling-name filter does not select `ci.yml-extern` itself;
the target sweep selects `target/`, not its source parent. No retained evidence
identifies Soldr or an external shell as the actor either. Historical process
and container ownership at the incident time cannot be reconstructed from the
current process table.

The investigation found concrete defenses that were missing:

- A non-checkout could be reclaimed on mtime alone. It is now pinned as
  `not a verified git checkout`; nested clones and bare repositories are never
  vouched for by a clean ancestor checkout. A checkout owning Git's linked
  worktree metadata is also retained.
- Sibling clones bypassed the extern-repo Git-state policy. Both kinds now
  receive off-worker cached verdicts and fresh pre-removal Git checks.
- Session cwds alone did not cover other host processes or container binds.
  Automatic source removal checks fresh host cwds and running Docker bind
  sources, including a child source or an ancestor bind. Missing process facts,
  failed Docker inspection, malformed mounts or incomplete results veto removal.
  A missing Docker executable means no Docker inspection is available on that
  host; no container operations are attempted. Containers are never removed by
  this source-protection check.
- Source-removal audit writes were best effort, and successful Git worktree
  removal had no direct audit event. Tracked-entry and worktree removals now
  require a persisted pre-action record naming the exact path, registry or Git
  evidence, and rule. Automatic legacy worktree removal also honors Git's dirty
  tree refusal: it never forces removal or recursively deletes after a refusal.
  The CLI's ancestry-driven cleanup also checks source use; without an explicit
  `--force`, it never recursively removes source left behind by Git. An audit write/sync failure refuses removal. Cache-only
  sweeps retain their existing best-effort logging policy.

The focused RED test is
`unknown_container_directory_is_pinned_even_when_stale`; before the fix it
reports `purge: true` for an uninspected directory. The tests also cover nested
checkout parents, common-Git metadata, live child cwds, child/ancestor binds,
unavailable snapshots, unrelated paths, and a genuinely clean pushed checkout.
Protection of uncommitted/unpushed work remains a Git-verdict requirement;
unknown or unreadable contents cannot be reclaimed as a shortcut. Restoring a
pathname never proves an old deleted-inode container bind points at the new
source: an overlapping running bind remains a veto until the runner is rebuilt.

## Two redb databases

| File | Concern | Ownership | Lifetime |
|---|---|---|---|
| `sessions.redb` (POSIX: `$XDG_STATE_HOME/clud/`; Windows: `%LOCALAPPDATA%\clud\`) | Per-launch session cap | File-lock serialized via `sessions.lock` | Opened for ms at startup and again at shutdown; never across the session |
| `~/.clud/data.redb` | Tracked-entry GC (`agent-*` worktrees, extern repos, sibling temp clones) | Single-owner: the `clud __daemon` process owns it via the in-process `daemon/gc_service.rs` registry-worker thread; everyone else uses JSON-over-TCP | Opened once per daemon lifetime; released after safe idle shutdown (900 seconds by default) |

Why two files? The session cap is a *guardrail* — it needs the simplest possible "open, decide,
close" semantics so a crashed `clud` can never deadlock the next one. The tracked-entry GC is a
*long-running registry* with reconcile/list/purge operations, concurrent insert traffic from the
scanner thread, and a CLI surface that benefits from being able to ask "what's tracked?" without
re-walking every repo.

Splitting them lets each pick the right concurrency model. Mixing them would force the daemon to
also be in the startup-cap critical path, which is a recipe for fork-bomb-with-a-twist when the
daemon hangs.

Both files use the redb invariant **one writer per process** (`flock` on POSIX, `LockFileEx` on
Windows). The two architectures below are different solutions to that one constraint.

## Session cap registry

Background: issue #73 — a buggy test spawned 100+ console windows from a single terminal. The cap
is a hard guardrail against that class of mistake.

The schema is a single `redb` table `sessions` keyed by `pid: u32` → JSON-serialized `SessionRow`
(`crates/clud-bin/src/session_registry.rs:94`, `crates/clud-bin/src/session_registry.rs:103`). A
sibling `meta` table records `schema_version` (`crates/clud-bin/src/session_registry.rs:97`).

Lifecycle on each `clud` launch:

1. **Acquire** the cross-process advisory lock at `sessions.lock` next to `sessions.redb`
   (`crates/clud-bin/src/session_registry.rs:613`). Blocks until exclusive.
2. **Open** the redb file (`crates/clud-bin/src/session_registry.rs:402`).
3. **GC** dead rows whose PID no longer names a live process, using `OsLivenessProbe`
   (`crates/clud-bin/src/session_registry.rs:471`). On Windows the probe is
   `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) + GetExitCodeProcess`; on POSIX it is
   `kill(pid, 0)`.
4. **Check** the cap (`crates/clud-bin/src/session_registry.rs:512`). Three outcomes: `Allow`,
   `Warn(n)` (at or above `cap/2`), or `Refuse(n)` (at or above cap).
5. **Register** this PID via `register_self` if not refused
   (`crates/clud-bin/src/session_registry.rs:520`). On `Refuse`, the row is **not** inserted —
   the caller exits, and inserting would inflate the count for the next sibling.
6. **Drop** the redb handle, then **release** the lock. The lock is held for a few ms — not for
   the lifetime of the session.

Shutdown re-acquires the lock briefly and removes the row via `unregister`
(`crates/clud-bin/src/session_registry.rs:546`). A crashed `clud` leaves a stale row that the
next startup's GC pass cleans up.

The whole startup sequence is wrapped by `run_startup_under_lock`
(`crates/clud-bin/src/session_registry.rs:677`) and the shutdown counterpart by
`run_shutdown_under_lock` (`crates/clud-bin/src/session_registry.rs:713`). The lock-then-redb
ordering is important: redb's file lock is released when its `Drop` runs, and we want that to
happen *before* the `sessions.lock` releases so the next sibling can open redb the instant it
acquires the advisory lock.

**Env overrides:**

- `CLUD_MAX_INSTANCES` (default 64; `0` disables the cap entirely;
  `crates/clud-bin/src/session_registry.rs:70`).
- `CLUD_WARN_INSTANCES` (default `cap/2`).
- `CLUD_SESSION_DB`, `CLUD_SESSION_LOCK` — test overrides for the file paths.

## Clud daemon (single-owner data.redb)

`~/.clud/data.redb` is held by **exactly one process at a time**: the lazily spawned
`clud __daemon` subprocess. It is not permanently resident: after 900 seconds without
owned work it exits and releases the database; the next ordinary daemon-using invocation
brings it back through `ensure_daemon`.
Issue #135 Phase 1 originally introduced this as a separate `gc_daemon`; [DD-012] folded it
into the existing session daemon so there's one daemon per user that hosts both `--detach` /
`attach` / `list` / etc. and the GC registry. The single-owner-of-redb invariant is unchanged —
only the owning process identity moved.

The merged daemon's GC half:

1. Binds a loopback TCP port on `127.0.0.1:0` (kernel-assigned, shared with session-management
   IPC).
2. Spawns one **registry worker thread** that owns the `Registry` handle and is the sole
   reader/writer of the redb file (`crates/clud-bin/src/daemon/gc_service.rs:spawn_registry_worker`).
   The session-management half of the daemon runs in the same process but on different threads;
   nothing else touches the redb handle.
3. Atomically writes `(pid, port)` JSON to `~/.clud/state/daemon.json`
   (`crates/clud-bin/src/daemon/server.rs`).
4. Serves connections forever: accept thread → per-connection thread →
   `mpsc::Sender<GcRequestMsg>` → worker thread → reply on a per-request `mpsc::sync_channel(1)`
   (`crates/clud-bin/src/daemon/server.rs::dispatch_gc_op`).

Wire protocol is JSON-over-loopback-TCP, one request per connection. The session daemon's
existing `DaemonRequest`/`DaemonResponse` enum gained two variants:
`DaemonRequest::Gc { payload: GcOp }` and `DaemonResponse::Gc { reply: GcReply }`. Inside,
`GcOp` carries `list` / `purge` / `reconcile` / `insert` (`crates/clud-bin/src/daemon/types.rs`);
`GcReply` carries `list_ok` / `purge_ok` / `reconcile_ok` / `insert_ok` / `error`.

**Creation-ledger rows (#1666).** `data.redb` also holds the `created` row
kind in its own `created_entries` table, keyed by `(session id, canonical
path)` (`gc::CreatedEntry`: kind `dir`/`file`, role, time, Unix dev/ino/uid).
`GcOp::insert_created` / `query_created` reach it through the same worker
(replies `created_insert_ok` / `created_rows_ok`); the periodic tick expires
rows older than 72h (`Registry::expire_created`). Their only reader is
`safe-rm`; see [rm-tools.md](rm-tools.md#creation-ledger).

`ensure_daemon(state_dir)` (`crates/clud-bin/src/daemon/client.rs`) is the idempotent bringup
entry point, called from `main.rs` on every clud invocation. It reads `daemon.json`, probes the
PID, and if alive + accepting TCP, returns. Otherwise it acquires `<state_dir>/daemon.lock`
(issue #138 — serializes concurrent bringup so two `clud` startups don't both spawn a daemon and
race on the TCP bind), **re-probes** under the lock, and only spawns if still absent.

Spawn is `clud __daemon --state-dir <state_dir>` detached via `daemon::client::spawn_detached_daemon` (running-process's daemon spawn, #1186)
with `invisible_helper_creationflags()` on Windows. Caller polls up to 5 seconds for the info
file plus a successful TCP connect.

### Idle lifetime policy

`~/.clud/settings.json` seeds `daemon.idle_timeout_secs` to `900`. Set a positive value to
choose a different timeout; set `0` to disable automatic retirement. The value is read at daemon
startup, not repeatedly on the listener path. Expiry is safe only when all of these are zero:
daemon workers, foreground-client leases, active TCP/HTTP/broker connections, and background
maintenance jobs. Each shutdown appends `daemon_idle_shutdown` with the timeout, elapsed idle
time, and every counter so an unexpected exit can be diagnosed from the event log.

[DD-012]: ../DESIGN_DECISIONS.md#dd-012-one-always-on-daemon-hosts-both-session-ops-and-the-gc-registry

## GC subcommands

The tracked-entry subcommands are thin IPC clients against the daemon. `--no-daemon`
is **an error**, not a fallback — there is no read-only path in v1
(`crates/clud-bin/src/gc/cli.rs`).

- **`clud gc list [--json]`** — `cmd_list` (`crates/clud-bin/src/gc/cli.rs`) calls
  `daemon::gc_client_list`, prints a table (or JSON). Each row reports
  `kind / age / agent_id / branch / state / path` plus a `live_locked` flag computed by the
  daemon by parsing `git worktree list --porcelain` and extracting `(pid <N>)` from any
  `locked <reason>` line.

  **Row state and reason (issue #896).** Once the extern-repo guard began refusing to
  reclaim checkouts holding local work, a pinned row became indistinguishable from garbage
  GC keeps failing to collect. Every row therefore carries a `state` —
  `reclaimable | pinned | dangling` — plus a short `reason` for the non-default states. The
  human table paints `pinned` yellow and `dangling` red; `--json` carries `state`, `reason`,
  `reclaimable` and `evaluated_unix` so tooling never parses ANSI.

  The load-bearing constraint is that **nothing on the registry worker thread runs the
  extern-repo git probe** — not `gc list`, not `gc purge`, not the periodic tick. That probe
  costs up to three subprocesses per checkout, and the worker is the single thread that owns
  redb, so every client op (including launch-path `record_repo_visit`) queues behind whatever
  it does.

  How verdicts are produced (issue #946):

  1. The periodic tick does the one thing only the worker can — `registry.list` filtered to extern-repo and sibling-clone rows —
     and hands the rows to `spawn_extern_probe` (`gc_service.rs`).
  2. A `clud-gc-extern-probe` thread computes `extern_repo_purge_verdict` for each row and
     sends the **complete snapshot** back as `RegistryMsg::ExternVerdicts`, reusing the same
     background-thread → worker channel that `RegistryMsg::PurgeCompletion` already uses for
     the purge pool.
  3. The worker installs it with `SpareReasons::replace_extern` — wholesale replacement, so a
     row absent from the snapshot loses its verdict and the cache can never exceed the row
     count. Only one probe runs at a time (`begin_probe`/`probe_finished`, owned by the loop
     rather than a `static`, so a test binary's many workers do not share it).
  4. `partition_purgeable` and `GcOp::List` then *look up* verdicts instead of computing them.

  **A row with no verdict yet is spared, never purged.** A fresh daemon, or a checkout added
  since the last probe, has nothing cached; falling back to the mtime gate would delete a
  checkout no probe has ever inspected, which is the data loss the extern-repo guard exists
  to prevent. The visible consequence is that a checkout becomes reclaimable on the tick
  *after* it is first seen clean.

  `gc list` additionally does a `try_exists()` stat per row for the `dangling` state, and a
  verdict can be up to one tick stale (1 h by default — hence `evaluated_unix`).

  **Dangling rows are forgotten each tick (#2095).** A `worktree`, `extern-repo` or
  `sibling-clone` row whose directory is gone used to be spared forever as "dangling: path
  missing", because its purge path re-checks the directory and finds nothing. The tick now
  drops these rows (`forget_dangling_source_rows`). Only the row goes, since nothing is left on
  disk. The size banner names `clud gc prune all` as the one command that reclaims what is safe.

  **A locked registry is retried, not given up on (#2095).** When `data.redb` is held by
  another process at daemon start (typically a superseded daemon that never exited), the
  worker answers GC ops with an error and retries the open every 30 s
  (`spawn_deferred_registry_worker`). Before this, the daemon ran without GC for its whole
  lifetime, so nothing was ever reclaimed and the size banners repeated on every exit.

  **The verdict is re-checked at delete time.** Decoupling put up to a tick between "this is
  clean and pushed" and `remove_dir_all`, and a developer can invalidate it in that window
  just by editing. So `reverify_before_delete` re-runs the verdict on the **purge-pool
  thread** — still off the worker, and immediately before the destructive call. A veto keeps
  the row and logs `spared … at delete time`. Before #946 this was implicit, because the
  verdict was computed inline microseconds earlier.

  Still *not* subprocess-free, and the doc should not pretend otherwise. Four worker-side
  paths still shell out through the deliberately-unbounded `worktrees::run_git`:
  `collect_live_lock_paths()` from `GcOp::List`, from `GcOp::DeleteById`, and from
  `partition_purgeable` (so twice per tick); plus `best_effort_branch` on the
  `GcOp::Reconcile` / `RegistryMsg::WatchRescan` paths. These are pre-existing and separate
  from the extern-repo probe that #946 moved.
- **`clud gc prune <kind> [--dry-run]`** and
  **`clud gc all [--dry-run]`** call `daemon::gc_client_purge` with the kind-specific safe
  prune policy. Worktree-like rows use the built-in stale duration, extern-repo rows rely on
  the cached probe verdict (recomputed at delete time), trash rows are reaped, and `uv-cache` uses its
  filesystem-only stale-env sweep.
- **`clud gc purge <kind> --yes [--dry-run]`** and
  **`clud gc all --purge --yes [--dry-run]`** are the destructive forms. The CLI requires
  a kind for single-kind purge and `--yes` for any destructive purge before contacting
  the daemon. Since #506 the kind is a positional argument (`--kind <name>` survives as a
  compatibility alias), and the pseudo-kind `all` routes prune/purge to the same
  every-managed-kind sweep as `clud gc all`: `clud gc purge all --yes` ≡
  `clud gc all --purge --yes`. The daemon selects candidates, partitions out live-locked worktrees or live
  session CWD ancestors, and:
  - `--dry-run` reports `removed` (the number that *would* be deleted) plus `skipped`. Replies
    `GcReply::PurgeOk { removed, skipped }` synchronously.
  - Non-dry-run **bulk** purge fans every purgeable entry out to the daemon's parallel purge pool
    (capped by `CLUD_GC_PURGE_CONCURRENCY`, default `min(num_cpus, 8)`) and returns
    immediately with `GcReply::PurgeStarted { dispatched, skipped }`. Each pool thread runs
    `remove_dir_all` / `git worktree remove --force` independently; on completion it sends a
    `RegistryMsg::PurgeCompletion(..)` back to the registry worker, which drops the matching
    redb row asynchronously. The redb writer never blocks on filesystem work — see #268.
  - Per-row delete (`GcOp::DeleteById`, dashboard "delete" button) stays on the synchronous path
    and replies `GcReply::PurgeOk { removed, skipped }` — exactly one entry, fast.
  Worktree rows use `git worktree remove --force`; if git fails or reports success while the
  directory survives, clud falls back to direct removal plus `git worktree prune`. Stale rows
  during/after a partial purge are tolerated — eventual consistency, not transactional; the
  next purge or list reconciles.
- **`clud gc reconcile`** — `cmd_reconcile` calls `daemon::gc_client_reconcile` with the
  current repo root. The daemon walks `<repo>/.claude/worktrees/`, `<repo>/.extern-repos/`, and
  conservative sibling temp-clone names next to the repo. Returns the count of new rows.

Bare `clud gc` (no subcommand) prints help and exits 0 without contacting the daemon
(`crates/clud-bin/src/gc/cli.rs`).

## Cached Rust target sizes (#1687)

`clud gc list --targets` groups the conventional `target/` directories of
known repository checkouts by repository. `--json` emits the snapshot's
`checked_unix`, `stale`, `truncated`, and `rows` fields; each row names its
repository, worktree, target path, byte count, and measurement status.
This reporting path also works with `--no-daemon`: it reads the last snapshot
without starting a daemon, running git, or walking a checkout.

The existing `clud-gc-repo-worktree-probe` background thread discovers the
checkouts and refreshes `target-sizes.json` in the daemon state directory.
The scan shares a 200,000-entry budget across the snapshot and caps the
snapshot at 2,000 worktrees and 4 MiB. Truncation and exhausted budgets are explicit.
Root and nested symlinks are not followed. Counts are apparent file lengths,
not physical disk usage; hardlinks in different trees may count twice.
Only conventional per-checkout `target/` paths are included; an external
`CARGO_TARGET_DIR` is not attributed to a checkout.

Missing directories, incomplete scans and read errors have a null byte count,
not a misleading zero. A timestamp older than six hours or more than five
minutes in the future is marked stale. A missing snapshot produces an empty
report; a corrupt snapshot reports an error. When repository probing is
disabled, the saved report remains available but stops refreshing.

Reporting neither registers deletion candidates nor changes the target sweep
or worktree-reclamation verdicts. `gc::target_size_cache` owns the cache and
reporting contract.

## Repo worktrees: squash-aware verdict (#1591) and reclaim (#1603)

Sibling worktrees such as `~/dev/clud2-wt-<issue>` match none of the tracked
kinds, and in a squash-merging repo their branch tips are never ancestors of
the default branch, so ancestry-based checks call them live work forever
([DD-122](../DESIGN_DECISIONS.md#dd-122-worktree-landed-verdicts-need-pr-or-patch-evidence-not-ancestry)).

- **Discovery.** Worktrees of repos already in the registry's repo-visit
  table (`record_repo_visit`), via `git worktree list --porcelain`; no
  filesystem crawl. Repos sharing a git common dir are probed once.
- **Shim hints (#2103).** A `clud` launch is not the only way a repo gets
  into that table. Each probe pass first tails the `git`/`gh` shim log
  (`<state>/logs/shim/git-gh.jsonl`) from the byte offset saved in
  `<state>/gc/shim-hints.offset` (`daemon/gc_service/shim_hints.rs`). For
  each logged call it resolves the `cwd`, and every argv token that names
  an existing directory, to the main checkout that owns it. It reads only
  the `.git`, `gitdir` and `commondir` files, and parses no subcommands.
  Repos found this way join the same pass. The worker records them with
  `Registry::note_repo`, which inserts a row with `run_count` 0 and leaves
  an existing row alone. A worktree made by any route that touched the
  shim is therefore listed within one scan. A bare repo, or a linked
  worktree of a submodule, cannot be named without git and is skipped.
- **Submodules (#2103).** Before each pass, every queued root's
  `.git/modules/**/config` is read for `core.worktree`, and each existing
  submodule checkout is queued as a root of its own, so a submodule's linked
  worktrees appear as `repo-worktree` rows (`shim_hints::submodule_checkouts`,
  file reads only, at most 512 module dirs per repo).
- **Filesystem backstop (#2103).** A repo made with an absolute `git` path
  or copied with `cp -a` never passes the shim. Each probe pass also
  advances a bounded crawl of the home directory (at most 50,000 entries;
  the pending stack persists in `<state>/gc/fs-crawl.json`, so a sweep
  spans passes and restarts). Every `.git` it meets is resolved with the
  shim resolver and registered as a hint. Symlinks are not followed;
  `node_modules`, `target`, caches and similar trees are skipped. A
  finished sweep repeats at most weekly (`daemon/gc_service/fs_backstop.rs`).
- **Resumable scan (#2103).** The pass walks a durable queue of roots,
  `<state>/gc/scan-queue.json`, rewritten atomically after every item; a
  scanned root moves to the back, so a daemon killed mid-scan resumes with
  the roots it had not reached. A pass stops after 5 minutes and the
  unreached roots keep their previous rows. A root whose `git`/`gh` call
  was killed at its deadline keeps its previous rows relabelled `pinned` /
  `scan timed out ×N` (never reclaimed on a timeout) and moves to the back;
  the count resets on its next clean scan. Each item remembers the row
  paths it last produced, because a row's `repo_root` is the main checkout,
  not the queued root (`daemon/gc_service/scan_queue.rs`).
- **Where it runs.** The worker reads the visit list and hands it to a
  `clud-gc-repo-worktree-probe` thread (`spawn_repo_worktree_probe`), which
  sends a complete snapshot back as `RegistryMsg::RepoWorktreeVerdicts`, the
  same off-worker pattern as the extern-repo probe (#946). One probe at a
  time; primed at startup, refreshed each tick. All git/`gh` calls are
  bounded `running-process` spawns. The probe thread also takes one
  process-table snapshot (`collect_process_cwds`, below).
- **Verdict.** `repo_worktree_verdict` (`daemon/gc_service/repo_worktree.rs`)
  is a pure function over `RepoWorktreeFacts`; its doc table is the
  precedence. Reclaimable only on positive evidence: a merged PR (one
  `gh pr list --state all` per repo per tick) whose `headRefOid` is the local
  tip or has it as an ancestor, or, with no PR, a closed PR, no `gh`, or a
  merged PR whose head does not cover the tip (rebased before its squash
  merge, #1961), the branch's cumulative `-U0` diff appearing verbatim as one
  default-branch commit. A commit made after the merge changes that diff, so
  a match still proves everything local landed.
  Spared, with a reason: `no verdict yet`, `main checkout`,
  `locked by live pid` (a lock with no parseable pid counts as live),
  `process inside` (a live session cwd or any process cwd),
  `process table unavailable`, `detached`, `dirty`, `untracked`,
  `open PR #N`, `commits after merge`, `no PR`, `closed PR #N`, `unverifiable`, and under
  `~/.clud/worktrees` only, `grace` (see the #1485 subsection below).
- **Orphaned `act` containers (#1878).** A local `act` run binds the worktree it
  tests, and an act killed mid-run leaves its container running after the worktree
  is gone. In delete mode the probe thread removes every `act-*` container with a
  bind source strictly under `~/.clud/worktrees` (or the legacy `tmp-wt`) that no
  longer exists, together with its own `<name>` and `<name>-env` volumes. Shared
  volumes such as `act-toolcache` are never touched. Incomplete `docker inspect`
  output removes nothing (`daemon/gc_service/act_containers.rs`).
- **Dangling admin entries (#1961).** In delete mode the probe thread runs
  `git worktree prune` for each repo whose `git worktree list` still names a
  missing worktree. Before this, prune ran only after a successful reclaim in
  the same repo, so these entries stayed forever. Prune is metadata only, and
  git skips locked entries.
- **Surfacing.** `clud gc list [--json]` appends `kind: repo-worktree` rows
  (`id: 0`, since no redb row backs them) with `state`, `reason`,
  `reclaimable` and `evaluated_unix`. A path the registry already tracks keeps
  its own row. A live session cwd inside pins the row at list time even if
  the snapshot is older.
- **Process table (#1603).** `collect_process_cwds` reads every visible
  process's cwd once (sysinfo) into a `ProcessCwdSnapshot`, and the pure
  `process_inside` decides against it, the `ProcessFacts` pattern from
  [process-reaping.md](process-reaping.md#daemon-sparing-os-signals-first-marker-second-whitelist-last).
  If the daemon cannot read its own cwd the snapshot is unavailable and every
  row spares as `process table unavailable`. The daemon's own `git` helpers
  (`own_git_helpers`) are ignored so a probe's `git status` does not pin the
  worktree it inspects.
- **Reclaim (#1603).** On each tick `run_repo_worktree_phase` walks the
  cached snapshot (taken by the *previous* probe) and, per the pure
  `reclaim_selection`, dispatches each `reclaimable` row with no live session
  cwd and no reclaim in flight to the purge pool as `PurgeWork::RepoWorktree`.
  The pool thread (`repo_worktree_reclaim_exec::run_reclaim`) re-probes that
  one worktree from scratch (fresh git, fresh `gh`, fresh process table) and
  `reverify_reclaim` vetoes on any change: verdict no longer reclaimable,
  branch changed, tip moved or unknown, worktree gone. Then, all through
  `running-process`: make read-only directories writable, `git worktree
  remove <path>` (**never** `--force`, so git itself refuses a tree that got
  dirty in the last instant), `git branch -D` only if the branch still points
  at the verified tip, optionally the remote branch (below), then `git
  worktree prune`. The outcome returns as `RegistryMsg::RepoWorktreeReclaimed`
  and is logged (`removed`, `spared at delete time: <reason>`, or `failed`);
  a removed row leaves the `gc list` snapshot. A failure is never retried
  with force: a locked worktree, or one with submodules, just stays.
  Rationale: [DD-123](../DESIGN_DECISIONS.md#dd-123-repo-worktree-reclaim-re-verifies-from-scratch-and-never-forces).
- **One reclaim per repo at a time (#1632).** The pool runs reclaims in
  parallel, but `run_reclaim_serialized` holds a per-repo mutex (keyed by the
  canonical repo root, `RepoLocks`) for the whole sequence above, so two
  worktrees of one repo are removed one after the other; different repos stay
  parallel. Concurrent git on one git dir failed with exit 255 on Windows.
  Reservation rows run no git and take no lock. Rationale:
  [DD-125](../DESIGN_DECISIONS.md#dd-125-repo-worktree-reclaims-are-serialized-per-repository-by-a-lock-on-the-pool-thread).
- **Remote branches.** Off by default. `gc.delete_remote_branches: true` in
  `~/.clud/settings.json` (seeded `false`), or
  `CLUD_GC_DELETE_REMOTE_BRANCHES=1`, also deletes `origin/<branch>`, only
  when the remote-tracking ref equals the verified tip, and the push carries
  `--force-with-lease=refs/heads/<branch>:<tip>`. `clud settings` exposes the
  setting as a toggle and notes when the env var overrides it (#1608).
- **Modes.** `CLUD_GC_REPO_WORKTREES`: unset or `1` deletes; `observe` (or
  any unrecognized value) probes and logs `would remove` without deleting;
  `0` turns the probe and the reclaim off.

### The worktree root `~/.clud/worktrees` (#1485)

- **Renamed from `~/.clud/tmp-wt` (#1962).** `tmp-` invited treating unpushed
  work as disposable. Each GC tick, before the worktree probe, the daemon
  moves what is left in the old root into the new one
  (`daemon/gc_service/legacy_worktree_root.rs`): git worktrees with
  `git worktree move` from their main repo, anything else by rename, a taken
  name with `-N`. Entries a live process is working in wait for a later tick,
  and nothing moves without a process snapshot. The empty old root is
  removed, and nothing is deleted. Until then, the `WorktreeRemove` hook
  accepts either root.

- **Location.** `gc::worktree_root::worktree_root()` is `~/.clud/worktrees`, a
  *sibling* of the `session_tmp` root `~/.clud/tmp`, never inside it, so the
  72 h mtime sweep cannot see a worktree by construction
  ([DD-124](../DESIGN_DECISIONS.md#dd-124-agent-worktrees-live-in-a-sibling-of-the-session-temp-root-and-are-never-on-its-timer)).
  It is created idempotently at session launch (`runner.rs`) and at
  GC-worker start, and is never a removal target: `git worktree remove` only
  removes the child it is given.
- **Allocation.** The bundled `grind-plan` and `clud-git` skills allocate new
  agent worktrees as `~/.clud/worktrees/<repo>-wt-<suffix>`, keeping the
  `<repo>-wt-` shape reconcile's name matching expects. Existing sibling
  worktrees are not moved; the discovery above still covers them.
- **Discovery.** Each direct child of the root seeds the probe as a repo
  root (`worktree_root_children`), so a worktrees worktree is judged even if
  clud never recorded a visit to its repo. Same verdict, same reclaim path.
- **Abandoned-empty.** Only under the root: a clean, idle worktree whose
  branch has zero commits past its merge-base with the default branch, and no
  open or merged PR, is `reclaimable` / `abandoned-empty` once its directory
  mtime is 24 h old, and `pinned` / `grace` before that (or when the age is
  unknown). Nothing else in the verdict consults age.
- **Ordinal allocator (#1486).** `gc::worktree_root::alloc_wt_path(slug,
  suffix)` reserves `<root>/<slug>-wt-<suffix>` with one atomic `create_dir`
  and, on a collision, takes `-2`, `-3`, ... in order, so the path it returns
  already exists and two callers never share one. `slug` and `suffix` must
  each be one plain path component (`InvalidInput` otherwise, and nothing is
  reserved).
- **Reserved-unused (#1486).** A direct child of the root that no `git
  worktree list` claimed is judged by `repo_worktree::reserved_dir_verdict`
  (pure, decision-table tested) instead: an **empty**, idle directory with no
  `.git` entry is `reclaimable` / `reserved-unused` after the same 24 h grace
  and `pinned` / `grace` before it. A non-empty one is `pinned` / `not empty`,
  one holding a `.git` entry is `pinned` / `unlisted checkout`, and neither is
  ever deleted by this rule. The purge pool re-probes it from scratch
  (`reverify_reservation`) and then calls a plain `remove_dir`, which the OS
  refuses for a directory that gained an entry in the meantime; no git and no
  recursive delete is involved.
- **Size backstop.** `worktrees.warn_bytes` in `~/.clud/settings.json`
  (seeded 50 GiB, `0` disables): `clud gc list` prints a stderr warning when
  the root exceeds it (a bounded, early-exit walk). Warn-only: nothing is
  deleted for size. The launch banner warns too (#1610) without walking:
  the daemon's repo-worktree probe thread runs the same bounded walk after
  each probe and writes `~/.clud/worktrees-size.json` (timestamp, limit,
  over/under/unknown + bytes; atomic temp-file rename). At launch
  `gc::worktree_size_cache::launch_warning` reads only that file and a
  lock-free settings peek, and prints one line naming the size, threshold,
  path and setting. It shows nothing when `warn_bytes` is `0`, the cache is
  missing, corrupt, older than 6 h or future-dated, or the walk ran out of
  entry budget (`clud gc list` still reports that case). The decision is the
  pure `banner_decision` table. If the probe never runs (reclaim mode `off`)
  the cache is never written and the banner stays silent. When the root is
  over the limit, the same thread also sizes each direct child (one shared
  entry budget) and writes the three largest to `~/.clud/worktrees-top.json`;
  the banner appends `; largest: <name> (<size>), …`, with `≥` for a child
  the budget cut short (#2101, `gc::worktree_top`).
- **Warn, then clean (#2101).** While the root is over `worktrees.warn_bytes`,
  each of those largest worktrees that is idle at least 24 h and not exempt
  (kept, in use by a session or process, reclaimable by the verdict, a
  reservation, or unknown to the probe) gets a removal date 7 days out in
  `~/.clud/worktrees-schedule.json`; the banner names it
  (`; moves to trash unless \`clud gc keep\`: <name> in <n>d`). The first
  probe after the date moves it to `clud trash` and registers the trash row,
  so it stays recoverable for 72 h. Becoming exempt, leaving the top list, or
  the root dropping under the limit removes it from the schedule. Observe
  mode only logs; no process snapshot means no scheduling
  (`gc::worktree_schedule` decides, `gc_service::worktree_schedule_exec` acts).
- **Dirty worktrees are never trashed (#2302 follow-up).** When the root is
  over budget, a schedulable candidate (non-exempt, idle >= 24 h) is probed
  once per tick with
  `git status --porcelain --untracked-files=all --ignored=no` (untracked new
  files count as dirty) and
  `rev-list --count HEAD --not --remotes` (the predicate `safe-rm` uses for
  clones, `rm_tool_clone::has_unpushed`). Uncommitted or untracked changes,
  unpushed commits, or any git failure (fail closed) make it exempt: it is
  logged as `kept: dirty <path> (<reason>)`, and a worktree already on the
  schedule that turns dirty drops off it (`worktree_schedule::mark_dirty`,
  `dirty_reason`, `Action::KeptDirty`). Exempt and recently used worktrees are
  never probed, so a sweep costs at most one probe per large idle worktree.
- **Size magnitude (#2302 follow-up).** The walk stops once past the limit, so
  `worktrees-size.json` `bytes` for `state: "over"` sits just above
  `warn_bytes`. Rather than spend a second crawl budget, the file now carries
  `"bytes_is_lower_bound": true` for `over` (false otherwise; older files
  without it read as false). Existing fields are unchanged; the per-worktree
  magnitudes live in `worktrees-top.json`.
- **Build-output prune first (#2302).** While the root is over the limit, each
  idle, non-exempt large worktree has its `target/` dirs (top-level and nested
  beside a `Cargo.toml`) pruned with `soldr cache prune-target <dir> --force`
  before scheduling; only superseded artifacts go, never source or git state.
  The size cache and top list are re-measured afterwards, and the schedule sees
  the root as under budget if the prune was enough. No soldr or a failed prune
  logs and falls through (`gc::target_prune`). Symlinks are never followed
  (each target must canonicalize inside its worktree), each soldr call is
  killed after 5 minutes, a target built within the last day counts as in use,
  the in-use check re-runs before every prune, and unchanged targets are
  skipped via `worktrees-target-prune.json` so idle sweeps cost nothing. An
  old soldr without `prune-target` is reported once per daemon run.
- **Keep list (#2101).** `clud gc keep [PATH]` (default `.`; `--remove`,
  `--list`) records a canonical path in `~/.clud/gc-keep.json`, no daemon
  needed. A kept path covers itself and everything below it. The tick skips
  kept rows, the purge pool re-reads the file before gathering facts and
  again immediately before mutation, and `clud gc list` shows them as
  `pinned: kept by user (clud gc keep)`. A corrupt file keeps everything and
  is never rewritten (`gc::keep_list`).
- **Tests.** Unit tests never read the real root: `production_worktree_root()`
  and `ensure_worktree_root()` return `None` under `cfg(test)`, and tests
  inject a tempdir root.

## Filesystem sweeps (non-registry)

Alongside the redb-tracked kinds, the daemon's periodic tick
(`run_periodic_purge_tick` in `crates/clud-bin/src/daemon/gc_service.rs`) runs four
**filesystem-only** sweeps that have no registry row — they operate directly on directories
under `~/.clud` (and, opt-in, on external dev roots). Each self-throttles via a sentinel
timestamp under `~/.clud/state/` so the per-tick cost is one stat + age compare.

| Sweep | Target | Age gate | Cadence sentinel | Enabled |
|---|---|---|---|---|
| uv-cache (#423) | `~/.clud/cache/uv/environments-v2/` | 72h | `uv-cache-sweep.last` (24h) | always |
| session-temp (#509) | `~/.clud/tmp/` | 72h | `session-tmp-sweep.last` (6h) | default on |
| session-state (#1014) | `~/.clud/state/sessions/` | 48h ambient / 30d notable | `session-state-sweep.last` (6h) | always |
| target (#510) | `target/` dirs under `CLUD_GC_TARGET_ROOTS` | 3d | `target-sweep.last` (24h) | opt-in |

**Session temp (#509).** At session launch the backend agent's temp env (`TMPDIR` on Unix,
`TMP`+`TEMP` on Windows) is pointed at `~/.clud/tmp` by both env builders
(`runner.rs::child_env` and `daemon/io_helpers.rs::child_env`) via
`crate::gc::session_tmp::env_overrides`, so the agent's temp scatter lands somewhere the daemon
can reclaim. Set `CLUD_SESSION_TMP=0` to keep the OS temp dir. If the dir can't be created the
override is silently skipped (the child keeps the OS temp dir) — launch never fails on this.

The session-temp sweep (#1260) starts on the six-hour cadence, but stores a work queue at
`~/.clud/state/session-tmp-sweep.work.json`. Its background worker continues and checkpoints
while work advances; it does not wait for another hourly GC tick or impose a deadline on a large
candidate. Deferred failures resume on a later tick. Candidate freshness scanning must finish before deletion, and a resumable
recheck precedes each destructive batch. Recent activity stops removal of the remaining tree.
File, directory, metadata-probe, scan, and exploration failures stay queued with backoff while
other candidates advance. A 72-hour
period of repeated failure without progress emits a nonfatal persistent-failure signal. An item
that fails `MAX_ITEM_RETRIES` (8, about 36 minutes of backoff) times in a row is dropped from
the queue and so spared; the next pass re-derives it from disk. A candidate that no longer exists
completes instead of failing. Owner-writable read-only directories inside an idle candidate are
made writable (`chmod u+wx`) and the unlink retried once; foreign-owned paths are skipped, see below, not
abandoned. Without the bound, one root-owned file kept the queue non-empty, a new pass never
started, and nothing that went stale later was ever looked at (#1672,
[DD-142](../DESIGN_DECISIONS.md#dd-142-a-session-tmp-sweep-pass-always-retires)).
**Unremovable subtrees (#2486).** A directory owned by another uid (typically `root` output of a
Docker bind mount into a scratchpad) or any path that returns `PermissionDenied` is terminal on the
first hit: the scan skips it without descending, its ancestor is kept (`Busy`) so Explore still
reaches removable siblings, and the path goes into the pass's `foreign_owned` set instead of the
backoff queue. A finished pass writes the list to `~/.clud/state/session-tmp-foreign-owned.json`;
`clud gc list` prints each path with a `sudo rm -rf` and a `docker run --rm -v <parent>:/x alpine
rm -rf /x/<name>` command. The next pass re-derives the list from disk. `daemon-events.jsonl` records per-tick examined entries, files and
directories removed, reclaimed apparent bytes, pending phases, retry count, work age, and the
last error path/class, current phase/path, and cursor position. An exclusive lock prevents
concurrent daemons from overwriting the continuation queue.

**Session temp size warning (#1327).** `tmp.warn_bytes` in `~/.clud/settings.json` (seeded
20 GiB, `0` disables) is the `~/.clud/tmp` twin of `worktrees.warn_bytes` and reuses its
mechanism in `gc::worktree_size_cache`: `clud gc list` prints a stderr warning from a bounded,
early-exit walk, and the daemon's maintenance sweep thread (after the age sweep, skipped on
`Defer`) writes `~/.clud/tmp-size.json`, a sibling of `tmp` so the sweep never sees it. At launch
`tmp_launch_warning` reads only that file and a lock-free settings peek, with the same
`banner_decision` table (stale after 6 h, quiet on unknown). **Warn-only:** nothing is deleted
for size; the 72 h age sweep stays the only deleter of session temp
([DD-141](../DESIGN_DECISIONS.md#dd-141-session-temp-size-is-reported-never-a-deletion-trigger)).

**Session temp size cap (#2397).** `tmp.max_bytes` (seeded 10 GiB, `0` disables) adds a size
trigger, superseding DD-141's warn-only stance for the over-cap case
([DD-194](../DESIGN_DECISIONS.md#dd-194-session-temp-is-capped-by-size-evicting-only-idle-units-oldest-first)).
`gc::session_tmp_cap::enforce_cap_at` splits the tree into eviction units with the age sweep's depth
rule (a depth-3 directory, i.e. `claude-<uid>/<project>/<session>/`, or a loose file; shallower
directories are recursed into, never removed whole) and evicts oldest-first until the tree fits.
**Liveness** is process/session signals plus idle mtime (it is not the age sweep's activity-only
rule): a unit is evicted only if its newest mtime is at least `MIN_IDLE` (6 h) old (unreadable,
oversized-walk or future-dated counts as live; re-measured right before removal) **and** no running
process has a cwd or `TMPDIR`/`TMP`/`TEMP`/`CLAUDE_CODE_TMPDIR` inside it, and no running process has a cwd (or ancestor of it) whose Claude-encoded form (`session_history::import::project_slug`) names a path component of the unit, so the whole `claude-<uid>/<project>/` subtree is live while anything works in that project (`TMPDIR` is the tmp root and claude's cwd the project dir, so only the encoded name links them; the encoding is JS-compatible, two dashes per non-BMP char, and a component also matches when both it and a live slug are >= 100 chars sharing their first 100, or it minus a trailing `-<hex>` (>= 100 chars) prefixes the live slug, covering Claude's believed-but-unverified long-path truncation with a hash, #2400; over-sparing is deliberate) **and** no session id the
daemon tracks (`state/sessions` not exited, `state/api-sessions` not terminated) is a component of
its path (`gc::session_tmp_live`). An unconsultable source makes every unit live, including a process scan where this process cannot read its own cwd/environ or any non-zombie process of the same user returned neither (a denied read looks like absence). A parked session
therefore survives however far over the cap the tree is, and `tmp.warn_bytes` (default = the cap)
warns when live sessions keep it above. The daemon runs it right after each age pass (`daemon/session_tmp_sweep.rs`, event
`session_tmp_cap`); `clud gc prune tmp` (and `prune all`) runs it on demand, honouring `--dry-run`.
Removal goes through `rm_tool::remove_dir_tree_writable`, the shared tree deleter.

**Undeletable paths are one line (#2397).** Container builds leave `root:root` trees (47 GB of CEF
under one `*-extern`) that the owning user cannot unlink. The gc deleters (`~/.clud/tmp` eviction,
trash reaping, tracked-entry removal) go through `rm_tool::remove_dir_tree_collecting`, which keeps
going past undeletable entries and reports each failed leaf to `gc::undeletable::Undeletable`. The
pass prints `Undeletable::summary_line()` once: `N paths could not be removed (permission denied),
e.g. <path>; owned by root — remove with sudo` (the owner clause comes from
`platform::rm_tool::file_owner`; mixed causes read `(D permission denied, O other)`). A tracked
entry whose tree failed returns that line as its error instead of one error per file.

**Dead worktree registrations (#2397).** A repo's `<common-dir>/worktrees/<name>/` entry
outlives its worktree directory when a container, a crash or a hand `rm` removes it; `git worktree
list` keeps printing it. `clud gc prune worktree-registrations` (also part of `prune all`) walks
every repo in the registry's `repo_visits` and runs plain `git worktree prune` (no `--expire=now`) through
`worktrees::run_git` (running-process). `gc::worktree_registrations::classify_entries` counts
candidates with plain reads: dead = the `gitdir` target is gone, the entry is not `locked`, **and
the worktree's parent directory exists**. A missing parent (unmounted volume) is "unreachable":
not counted dead, and its repo is not pruned (git cannot skip single entries). Entries still dead
after the prune are reported "dead, pending git expiry". The count is exact, `--dry-run` reports it without pruning, and two roots sharing a common
dir count once. Metadata only: no worktree directory is ever touched. `clud gc list` prints the
dead count on stderr (so `--json` stays parseable). `purge` runs the same pass.

**Cache size warning (#1691).** `cache.warn_bytes` (seeded 20 GiB, `0` disables) applies the
same mechanism to `~/.clud/cache`, which is almost entirely uv's own cache under `cache/uv`:
`clud gc list` walks it (bounded), the maintenance sweep thread writes `~/.clud/cache-size.json`,
and `clud_cache_launch_warning` reads only that file at launch. The warning itself never deletes,
and clud does not run `uv cache prune` (which also drops every `environments-v2` env)
([DD-144](../DESIGN_DECISIONS.md#dd-144-cludcache-size-is-reported-clud-does-not-run-uv-cache-prune)).

**Cache size cap (#1691).** `cache.max_bytes` (seeded 32 GiB, `0` disables): after its age pass,
the daily uv sweep (`daemon/uv_cache_sweep.rs`) calls `gc::uv_cache::enforce_cap`, which runs
`uv cache clean --cache-dir ~/.clud/cache/uv` when the cache is over the cap. clud deletes no
bucket itself. The pure `decide_cap` spares when any `uv` process is running, the root is a
symlink or not directly inside a real `~/.clud/cache`, or `UV_LINK_MODE=symlink`. Each clean is
audited as `gc.uv-cache-cap`
([DD-145](../DESIGN_DECISIONS.md#dd-145-the-uv-cache-is-capped-by-uv-cache-clean-never-by-deleting-buckets)).

**Corrupt wheel repair (#1711).** Before the cap, the same sweep (and
`clud gc prune --kind uv-cache`) runs `gc::uv_cache_repair::repair_corrupt_wheels_at`. It finds
live `wheels-v*` pointers (`<key>` symlink plus `<key>.http`/`.rev`) whose `archive-v0` target has
no `.dist-info`, no `RECORD`, or a missing `RECORD`-listed file. Under a shared `<root>/.lock` and
uv's exclusive `<key>.lock` it unlinks only the pointer files, so uv refetches. A busy entry, a
running `uv cache clean`, or a pointer uv republished after the scan is skipped. The archive is never
touched. Each removed file is audited as `gc.uv-cache-repair`. Unix-only in effect: on Windows uv
writes `<key>` as a link file, not a symlink
([DD-148](../DESIGN_DECISIONS.md#dd-148-a-corrupt-uv-wheel-entry-is-invalidated-under-uvs-entry-lock-not-cleaned)).

**Trash retention and size cap (#1340, #1672).** `~/.clud/trash` holds `clud trash` quarantine
entries (reaped on the next GC tick) and `safe-rm` entries (they carry `TRASH_MANIFEST`, kept for
`rm_tool::TRASH_KEEP` = 72 h so they can be restored by hand). The tick's reaper makes a tree
owner-writable before `remove_dir_all`, as safe-rm's purge does (#1573), logs each failure with
its OS error, and drops a registered row whose directory is already gone. Files owned by another
user (root, from a Docker bind mount) still cannot be removed by the daemon; they are logged on
each tick and need a `sudo rm` or a container-side `chown`. On top of the time window,
`trash.max_bytes` in `~/.clud/settings.json` (seeded 20 GiB, `0` disables) caps the total: the
maintenance sweep thread (`daemon/trash_cap.rs`, skipped on `Defer`) evicts the oldest real
directories directly inside the canonical trash root until the total fits, never one trashed less
than an hour ago, never following a symlink, auditing each removal as `gc.trash-cap`. Entry sizes
are walked once per daemon lifetime, since entries do not change. `clud gc purge trash --yes`
empties registered entries on demand
([DD-143](../DESIGN_DECISIONS.md#dd-143-trash-has-a-size-cap-session-temp-does-not)).

**Forensic session state (#1014).** Every launch leaves a `<pid>__<start-epoch>/` directory under
`~/.clud/state/sessions/` holding the reaper's `reap.jsonl` / `reap-health.json` and, since #1011,
a `bridge.jsonl`. Nothing aged it out, so it accumulated one entry per session ever run — the
sibling `state/launches/` tree has been bounded by `MAX_RECORDS = 200` since #998. The entries are
tiny; the cost is directory-listing time and a tree nobody can search by hand during an incident.

Two windows, because the whole point of #998 and #1011 is that a failure trail survives to be read
afterwards. `crate::gc::session_state::classify` reads the directory's `bridge.jsonl` and calls it
**ambient** if every record is one the bridge wrote through `record_ambient` (`catalog_advertised`,
`admission_queued`, `admission_acquired`, `model_substituted` — see `AMBIENT_EVENTS`), **notable** otherwise. Ambient
directories go at 48h, notable ones at 30d. Anything unreadable, unparseable, or carrying an
unrecognized event counts as notable: drift and corruption both fail toward *keeping* the trail,
since deleting a forensic log on a parse guess is the only unrecoverable mistake available here.
`ambient_event_names_match_record_ambient_call_sites` fails if a new `record_ambient` event is
added without listing it.

A **live session's directory is never swept**, at any age — the reaper and the bridge write into it
for the whole life of the launch. Liveness is injected from `daemon/session_state_sweep.rs` rather
than called inside `gc::session_state`, which keeps the policy module free of process introspection
and its decision table unit-testable. A recycled PID can therefore keep one small directory alive
until that unrelated process exits; that is the safe direction and costs a few hundred bytes.

**Target reclamation (#510).** Opt-in: does nothing unless `CLUD_GC_TARGET_ROOTS` names one or
more dev roots (OS path-list separated). The sweep walks each root (bounded depth, skipping
`.git`/`node_modules`/`.claude` and not descending into a found `target/`), and removes `target/`
dirs — identified by a sibling `Cargo.toml` — whose mtime is older than
`CLUD_GC_TARGET_STALE_DAYS` (default 2). Default-off because reclaiming `target/` forces a
rebuild; the long mtime gate is the cheap stand-in for "no live build owns this."

**Background thread + prioritization.** The session-temp, session-state and target sweeps walk the filesystem
and can take a while, so the tick spawns them on a detached `clud-gc-sweep` thread rather than
blocking the registry tick loop; an `AtomicBool` guard prevents overlapping sweeps. Priority
(`maintenance_action` in `gc_service.rs`):

- **Disk low** — free space on the `~/.clud` volume or any target root below the watchdog warn
  threshold (`CLUD_GC_WARN_FREE_GB`): reclaim immediately, bypassing the sentinel throttle.
- **Otherwise** — only run when global CPU is under `CLUD_GC_SWEEP_MAX_CPU_PCT` (default 60%);
  if the box is busy, skip this cycle and let the next tick retry.

## Worktree scanner

`WorktreeScanner` (`crates/clud-bin/src/gc/scanner.rs`) is a polling thread spawned at clud startup
from `main.rs` via `WorktreeScanner::maybe_spawn*()`. It walks `<repo>/.claude/worktrees/`,
`<repo>/.extern-repos/`, and conservative sibling temp-clone names every ~2 seconds and sends
`gc.insert` IPC ops for matching immediate subdirs it sees. The scanner is
**insert-only**: rows that already exist are no-ops (`Registry::insert_if_new` at
`crates/clud-bin/src/gc/registry.rs`), so there is no per-cycle write churn.

Sleep is **chunked** — short 25ms sleeps add up to the ~2s polling interval, but remain cancellable
quickly — so Ctrl+C teardown does not block for two seconds waiting for the next iteration
(`crates/clud-bin/src/gc/scanner.rs`).

Cancellation is cooperative via `Arc<AtomicBool>`. `Drop` (`crates/clud-bin/src/gc/scanner.rs`) joins
the thread; the startup-side guard `_scanner_guard` in `main.rs` triggers this on normal
shutdown.

If the daemon is unreachable, the scanner logs once (debug level, gated by
`CLUD_GC_SCANNER_VERBOSE`) and **stops trying for the rest of the session**. It does not retry
on a backoff. This is intentional: in single-user CI/dev contexts there's no daemon to contact,
and quiet failure is preferable to a 2s-per-cycle stream of error logs.

## `--clean-worktrees`

`--clean-worktrees` (`crates/clud-bin/src/worktrees.rs`) is **unrelated to the GC store**. It is
a one-shot CLI flag for cleaning up git worktrees in the current repo, with no `redb`
involvement.

It enumerates via `git worktree list --porcelain`, classifies each entry as
`clean / dirty / unpushed / no-upstream / branch-gone` (`crates/clud-bin/src/worktrees.rs:53`),
and removes those that are *safe* — clean AND (older than `--stale-after` OR upstream `[gone]`).
`--force` widens the safe set to include `dirty` and `unpushed`.

Locked worktrees with fresh live/unknown/dead lock reasons are skipped. Once a lock exceeds
`CLUD_GC_LOCKED_HARD_AGE_DAYS` (default 7), `--clean-worktrees` presumes it is orphaned and lets
the entry pass through the same clean/dirty/unpushed/no-upstream/force rules as an unlocked
worktree. `--dry-run` is a faithful preview: nothing is mutated until the actual verified
worktree removal path.

**Shared landing verdict (#1606).** Ancestry cannot see a squash merge (DD-122), so before its
own rules the CLI runs the daemon's probe and `repo_worktree_verdict` for every worktree of the
repo (`crates/clud-bin/src/worktrees_verdict.rs`, through the bridge
`crates/clud-bin/src/daemon/gc_service/repo_worktree_cli.rs`), with one `gh pr list` per repo
and the patch-match fallback when `gh` is unavailable. Precedence, first match wins:

| condition | action |
|-----------|--------|
| lock too fresh for the hard-age gate | skip, as before |
| verdict `reclaimable` and ancestry status not `dirty` | remove via the daemon executor, reason = verdict reason (no `--force` needed) |
| any other verdict, ancestry skips | skip, reason gains `; verdict: <reason>` |
| any other verdict, or no probe row | the ancestry action, unchanged |

So the verdict only adds removals backed by positive evidence (merged PR covering the tip, patch
match, abandoned-empty under `~/.clud/worktrees`); every spare verdict (`dirty`, `untracked`,
`open PR #N`, `commits after merge`, `no PR`, `locked by live pid`, `process inside`, `detached`,
`main checkout`, `unverifiable`, ...) leaves `--force`, `--stale-after` and the lock rules exactly
as documented above. Verdict-backed removals go through `run_reclaim_with`
([reclaim](#repo-worktrees-squash-aware-verdict-1591-and-reclaim-1603)): fresh re-probe with an
unchanged verdict and tip, `git worktree remove` never `--force`, `branch -D` only at the verified
tip, prune, never a remote delete. A veto at that re-check is reported as `skipped (verdict
changed before removal: ...)`, not a failure. `--dry-run` prints the same plan, verdict and
reason; the status table shows `[state: reason]` per worktree.

**Verdict deadline (#1648, DD-130).** The verdict phase is bounded: a repo with only its main
checkout skips it entirely (no process table, no `gh`); otherwise `spawn_cli_probe` gathers rows
on a worker thread (one process-table snapshot, at most one `gh pr list` with a 3 s timeout and
only if some worktree reaches the PR check, git probes one worktree at a time) and streams them
back. `plan_in` waits at most `VERDICT_DEADLINE` (4 s) in total. A worktree whose row has not
arrived keeps the ancestry decision via `decide_verdict_timed_out`: a skip gains `; verdict timed
out`, an ignore becomes `skip (verdict timed out)`, and an ancestry removal stays as it was before
#1606. A missing verdict never produces a verdict-backed removal. `--dry-run` goes through the same
path, so it shows the timeout reason a real run would act on.

**No lock against a running daemon.** The per-repo reclaim lock (#1632, DD-125) serializes daemon
pool threads only. The CLI does not take it: its re-probe turns a worktree the daemon already
reclaimed into a skip (`gone from git worktree list`), and a simultaneous git step contends on
git's own `.lock` files and fails without forcing anything (DD-129).

The GC daemon also borrows `parse_worktree_porcelain` and the "extract pid from locked-reason"
helper from this module to compute the `live_locked` flag on `gc.list` output.

## Key types

Session cap:

- `SessionRegistry` (`crates/clud-bin/src/session_registry.rs:358`) — open redb handle plus
  own-pid + liveness probe.
- `CapConfig`, `CapDecision` (`crates/clud-bin/src/session_registry.rs:171`,
  `crates/clud-bin/src/session_registry.rs:191`) — pure cap-check inputs/outputs.
- `SessionInfo`, `SessionRow` (`crates/clud-bin/src/session_registry.rs:206`,
  `crates/clud-bin/src/session_registry.rs:103`) — public + on-disk row shapes.
- `LockGuard` (`crates/clud-bin/src/session_registry.rs:602`) — RAII for `sessions.lock`.

GC store / daemon:

- `Registry` (`crates/clud-bin/src/gc/registry.rs`) — owns the redb handle inside the daemon's
  registry-worker thread.
- `TrackedEntry`, `InsertInput` (`crates/clud-bin/src/gc/registry.rs`) — public row + insert-input
  shapes.
- `WorktreeScanner` (`crates/clud-bin/src/gc/scanner.rs`) — polling thread that talks IPC to the
  daemon.
- `DaemonInfo` (`crates/clud-bin/src/daemon/types.rs`) — info-file shape (pid + port) shared
  with the session-management half of the daemon.
- `ListRow` (`crates/clud-bin/src/daemon/types.rs`) — public JSON row shape returned by
  `gc.list` and serialized by `clud gc list --json`.
- `GcOp`, `GcReply` (`crates/clud-bin/src/daemon/types.rs`) — IPC payload enums carried inside
  `DaemonRequest::Gc` / `DaemonResponse::Gc`. `GcReply::PurgeOk { removed, skipped }` is the
  synchronous outcome (dry-run + DeleteById); `GcReply::PurgeStarted { dispatched, skipped }`
  is the bulk-purge fan-out outcome.
- `RegistryMsg`, `GcRequestMsg`, `PurgeCompletion`, `PurgeJob` (`crates/clud-bin/src/daemon/gc_service.rs`)
  — the worker's internal mpsc carries both client ops (`RegistryMsg::Op(GcRequestMsg)`) and
  fire-and-forget purge-pool callbacks (`RegistryMsg::PurgeCompletion`). The pool's job queue
  is `mpsc::Sender<PurgeJob>`.

`--clean-worktrees`:

- `WorktreeEntry`, `WorktreeStatus` (`crates/clud-bin/src/worktrees.rs:30`,
  `crates/clud-bin/src/worktrees.rs:53`).
- `CleanOptions` (`crates/clud-bin/src/worktrees.rs:91`).

## Failure modes

- **Daemon crash mid-request.** Client's TCP `read_line` returns 0 bytes; `gc_client_*`
  surfaces as `io::Error`. CLI prints `error: <op> failed: <msg>` and exits 1. The next
  `ensure_daemon()` from any clud process re-spawns the daemon. The worker's
  `recv_timeout(WORKER_REPLY_TIMEOUT)` in `dispatch_gc_op`
  (`crates/clud-bin/src/daemon/server.rs`) prevents a wedged worker from hanging the accept
  thread indefinitely.

- **Bringup race.** Two `clud` startups call `ensure_daemon()` simultaneously. Both see no
  daemon. Both try to acquire `daemon.lock`. The winner spawns; the loser blocks, then
  re-probes under the lock, finds the daemon, and returns. Issue #138.

- **`sessions.redb` lock contention.** N concurrent `clud` launches serialize on `sessions.lock`,
  never on the redb file lock. The lock is held for ~ms per launch; even hundreds of
  simultaneous spawns drain in well under a second on the host.

- **`sessions.redb` corruption.** `Database::create` returns a `redb::DatabaseError`;
  `run_startup_under_lock` surfaces it; `main.rs` logs and skips the cap check rather than
  refusing to launch (the cap is best-effort safety, not a hard requirement). The user can `rm`
  the file to recover.

- **Scanner thread panic.** The scanner thread is independent of the main launch path; a panic
  logs to stderr but does not affect the running session. `Drop::cancel` joins, so a panic in
  the worker also won't leak the thread.

- **Worktree dir gone mid-scan.** `read_dir` returns `NotFound` → `scan_once_via_ipc` treats as
  empty (`crates/clud-bin/src/gc/reconcile.rs`). Individual `agent-*` dirs disappearing between
  `read_dir` and the `gc.insert` IPC are benign: the daemon will accept the insert, and a future
  `gc prune`, explicit `gc purge --kind`, or a `git worktree remove` from outside clud cleans
  the row.

- **PID reuse.** The session-cap registry's `Drop` only deletes its row if `register_self` was
  called successfully (`crates/clud-bin/src/session_registry.rs:565`), so an early-aborted `clud`
  cannot clobber a sibling that happened to inherit its PID via POSIX PID reuse. `unregister`
  clears the flag too, for the same reason.

- **`--no-daemon`.** All `clud gc *` subcommands exit 2 with "gc
  operations require the clud daemon; remove --no-daemon". The scanner's IPC fails silently
  and the thread idles for the rest of the session. The session cap is unaffected (it doesn't
  use the daemon). The always-on auto-spawn from `main.rs` is also skipped.

## See also

- [`daemon-ipc.md`](daemon-ipc.md) — the always-on `clud __daemon` hosts both interactive
  session ops (`Create` / `Session` / `Terminate`) and GC ops (`Gc { payload }`) on the same
  loopback TCP listener.
- [`../DESIGN_DECISIONS.md`](../DESIGN_DECISIONS.md) — DD-006 introduced the "redb is
  single-owner" rule; DD-012 records the merge of `gc_daemon` into the session daemon while
  preserving that invariant.

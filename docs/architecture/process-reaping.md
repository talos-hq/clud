# Process reaping

How clud cleans up processes it left behind, and — more importantly — how it
decides what it must **not** touch.

Reaping spans `job_orphan_reaper.rs`, `orphan_reaper.rs`, `process_scan.rs`,
`process_tree.rs`, `process_identity.rs`, `reap_log.rs`, `daemon/proc_sampler.rs`
and the `running-process` boundary, so the cross-directory story lives here and
the per-directory READMEs link in.

Rationale for the non-obvious choices: [DD-021], [DD-023], [DD-024].

---

## The two reapers are disjoint

They look like duplicates. They are not, and misreading them produces a false
safety argument — one was made during #673's own review.

| | `job_orphan_reaper` (Windows) | `orphan_reaper` (cross-platform) |
|---|---|---|
| **Selects by** | Job Object membership + a role classification of the process tree | inherited `RUNNING_PROCESS_ORIGINATOR=CLUD:<pid>` tag |
| **Catches** | leaks of a **live** clud — a tool shell exited and left a client running | orphans of a **dead** clud — the session that spawned them is gone |
| **Runs** | continuously, off a completion port, at up to 5 Hz | at foreground exit, on `clud slay`, and on the daemon's periodic sweep |
| **Key filter** | the trigger shell's exit | `!parent_alive` (`orphan_reaper.rs`) |

The daemon's periodic sweep filters `!p.parent_alive`, so it sees **only**
orphans whose originating clud is dead. The job reaper exists precisely for
leaks of a clud that is still running. **The two sets are disjoint by
construction**, which is why neither can serve as the other's backstop — and why
the job reaper needs its own retry-at-exit list for exits it abandons
(#673 Phase 2d).

---

## Explicit Command Prompt windows on Windows (#2007)

The generated Bash startup file installs a Windows adapter for exactly
`start cmd` and `start cmd.exe` (case insensitive). It invokes the hidden
`clud __start-cmd` fast path; other argument forms use stock Git Bash `start`.
The adapter remains present when nounset is disabled.

The helper uses running-process's breakaway daemon launch with inherited
environment and cwd to run `cmd /D /C start "" cmd.exe`. Breaking away leaves
the inner PTY kill-on-close job. The new console inherits
`RUNNING_PROCESS_IS_DAEMON=1`, so the existing `DeclaredDaemon` reason also
protects it from foreground exit cleanup and later orphan sweeps. The outer
foreground tracking job has no kernel kill-on-close limit. Its explicit exit
sweep reads declared-daemon facts through the same identity-keyed marker cache
as the live reaper; passing an empty marker set there would kill the console
after a successful PTY breakaway.

A reaper-only exemption cannot protect a process still inside the PTY job.
No blanket `cmd.exe` exemption is added: `/c` wrappers, `/b`, titled launches,
and other applications retain their existing behavior. Native ConPTY tests
cover both bare aliases, real cleanup, marker production, and stock fallback.
See [DD-177](../DESIGN_DECISIONS.md#dd-177-explicit-command-prompt-windows-use-the-existing-persistence-contract).

---

## The `(pid, creation_time)` keyspace

A bare PID is not a stable handle. Windows reissues numbers promptly, so "the
PID is alive" and "that process is alive" are different questions
(`process_identity.rs`). Every per-process cache, set and log in the reaping
subsystem is therefore keyed by `ProcessIdentity { pid, start_time }`.

Two populations, because a key can be born incomplete:

- **Keyed entries** — a full `(pid, creation_time)`. Purged by the single sweep
  once the process is confirmed gone and its grace window has elapsed.
- **Unkeyable entries** — the process died before a creation time could be read.
  No amount of retrying will produce a key, so they get a **hard TTL and a hard
  cap** rather than an eviction rule tied to liveness.

**One purge sweep** (`TrackerProcesses::purge`) evicts everything together:
`known`, `processed_exits`, `provisional_empty_exits`, the retry counters, the
deferral timestamps and the unkeyable pen. Two invariants govern it:

- *Never evict an interior dead node that still has a live descendant.* The
  planner walks `parent_pid` links **through** `known`, so removing an interior
  node disconnects — and silently stops reaping — its whole subtree.
- *Never evict a companion before `known`.* Dropping `processed_exits[(pid, st)]`
  while a dead `known[pid]` survives makes `pending_exit_pids` resurrect the
  identity and re-finalize it forever.

Every kill re-verifies the identity immediately before acting — **every**
target, root and descendant alike. Between the scan that selects a target and
the kill that acts, a PID can die and be recycled, and killing a *tree* takes
the replacement's children with it. `TopologySnapshot::kill_tree_filtered` used
to re-check only the root and then kill descendants straight from the snapshot
on a bare PID; #688 closed that, generalizing what Windows'
`kill_tree_filtered_automatic` already did.

### Parent links go stale too

Windows never rewrites a process's parent PID when the parent exits. Once that
PID is recycled, the orphan names the new holder as its parent, so a raw
`parent()` map makes an older, unrelated process look like a child. #1738: an
ancestor of the CI pytest runner had a dead parent, a clud daemon was later
handed that PID, and the integration suite's tree kill of the daemon walked up
into the runner and killed pytest (exit 1, no summary). On a desktop the same
walk reaches `explorer.exe`, whose parent `userinit.exe` exits at logon.

Every kill-path walk therefore builds its index through
`process_tree::children_index`, which links a child only when it started no
earlier than the process now holding its parent PID (and drops a child whose
start time cannot be read). The daemon's `signal_process_tree` and the test
suite's `tests/process.py::terminate_process_tree` use the same rule. sysinfo
start times are whole seconds and a PID is often recycled within the second, so
on Windows a same-second link is settled by exact creation times
(`process_identity::creation_ticks`).

---

## Daemon-sparing: OS signals first, marker second, whitelist last

A clud session must never kill a daemon started inside it. Which daemon it is
does not matter; **how it detached** does.

Every signal sits behind the `ProcessFacts` trait, is collected once per pass
into a `FactsSnapshot`, and is then consulted as pure data. No reap-decision
code calls Win32 — see [DD-024]. The trait, the precedence order and the
per-platform producers live in `reaper_facts.rs` and
`platform/{windows,unix}/reaper_facts.rs`.

### Which reaper consults the table

**Both of them, and that is new.** #673 Phase 1a landed the table inside
`job_orphan_reaper`, so for a while only the Windows tool-shell reaper had it.
`orphan_reaper` — the cross-platform one, with the wider blast radius — still
spared by the cooperative marker alone. Net effect: an sccache-shaped daemon
survived the tool shell that started it and was then killed at clud's own exit,
by `clud slay`, or by the daemon's periodic sweep. #688 moved the table into
`reaper_facts.rs` and wired the second consumer.

| Consumer | Entry points | Signals it can answer |
|---|---|---|
| `job_orphan_reaper` (Windows) | tool-shell exit, runtime reconcile, exit sweep | all seven — it owns the Job Object handle, so row 1 is available |
| `orphan_reaper` (cross-platform) | foreground `clud` exit, `clud slay`, daemon `ReapOrphans` + periodic sweep | rows 2–7; **row 1 is reported unavailable**, because there is no job on this path |

Row 1 being *unavailable* rather than answered "inside the job" is load-bearing:
answering it would read as a positive finding of containment for every candidate.

The spare decision carries a **reason**, and `ReapOutcome::spared` surfaces it
to the caller. "The daemon survived" is not the property worth asserting — a
reaper that never saw the process at all also leaves it running.

The same collect-once-then-decide shape guards directory deletion too: repo-worktree
GC snapshots every process cwd (`ProcessCwdSnapshot`) and a process inside a
worktree pins it. See
[gc-and-registry.md → Repo worktrees](gc-and-registry.md#repo-worktrees-squash-aware-verdict-1591-and-reclaim-1603).

### Precedence

| # | Signal | Windows | POSIX | Catches |
|---|---|---|---|---|
| 1 | **Job-object membership** | `IsProcessInJob` — false ⇒ broke away | — | anything using `spawn_daemon_breaking_away_from_job`; cheapest and strongest, we already own the job handle |
| 2 | **Session / service context** | `ProcessIdToSessionId` == 0 ⇒ service | — | **dockerd** / Docker Desktop |
| 3 | **Session leader** | — | another session than the reaper's that the candidate itself leads (`getsid(pid) == pid`) | detached session leaders |
| 4 | **Token owner** | process cannot be opened for termination | `euid` differs | services; also cases where the kill would fail anyway |
| 5 | **Declared daemon** | `RUNNING_PROCESS_IS_DAEMON` | same | **zccache**, **soldr** — everything that opted in |
| 6 | **Listening endpoint** | `GetExtendedTcpTable` over `AF_INET` **and** `AF_INET6` | `/proc/net/tcp{,6}` + `/proc/net/unix`, matched to `/proc/<pid>/fd` | **sccache**, **FBuildWorker**, language servers |
| 7 | **Active desktop shell** | `GetShellWindow` + `GetWindowThreadProcessId` | unavailable | Explorer intentionally restarted to restore the taskbar (#746) |
| 8 | **Docker Desktop family** | `Docker Desktop.exe` / `com.docker.*` roots | same | Docker Desktop backend plus its WSL runtime subtree (#773) |
| 9 | **Configured spare-list** | `CLUD_REAPER_SPARE_IMAGES` | same | operator escape hatch; ships empty |

On POSIX, a live session leader does not by itself protect the other
processes in its session. Tagged workers can inherit a session from a PTY or
wrapper and are evaluated for their own daemon signals.

Cheap and authoritative first, expensive last. 1–4 are one syscall each and no
memory read, and every PID they rule out is a PID whose environment is never
touched. 6 is evaluated only for what survives them.

Known gap: **Windows named pipes**. Windows exposes no documented
pipe-name-to-owning-PID mapping, so a daemon whose only endpoint is a named pipe
still needs the marker or the operator spare-list. The POSIX analogue is
covered — `/proc/net/unix` is read alongside the TCP tables.

The active-desktop-shell signal is deliberately narrower than a GUI exemption:
`GetShellWindow` identifies Windows' active shell window, and
`GetWindowThreadProcessId` identifies its owner. Only that owner is spared, and
only while it remains a tracked candidate, so a restarted Explorer survives the
tool shell that launched it without protecting unrelated visible applications.

Coverage is not uniform across platforms, and the table says so rather than
guessing: on macOS only rows 3 and 5 are answerable without linking a
platform-specific process-inspection library, so rows 2, 4 and 6 are reported
unavailable there.

A signal the platform cannot answer is recorded as **unavailable** and never
spares. Absence of evidence is not evidence of daemon-hood — that inversion is
exactly what #522 fixed.

**Console attachment is deliberately unranked.** Once the trigger shell exits
its console goes with it, which makes "no console" indistinguishable between a
detached daemon and an ordinary leaked client. It would over-spare precisely
when it is consulted.

A whitelist is a **last resort and must be data, not code**. The one deliberate
exception is Docker Desktop's narrowly identified product family (#773): its
backend is user-session scoped and commonly exposes only a Windows named pipe,
so none of the generic service signals can prove that it and its WSL subtree
belong to a durable engine. The reaper spares only the official UI/backend
roots (`Docker Desktop.exe` and `com.docker.*`), then its existing prune rule
keeps their descendants intact; it does not spare the ordinary `docker.exe`
client.

### The cooperative-marker caveat

> **`RUNNING_PROCESS_IS_DAEMON` is set by *other programs*, not by clud.
> Grepping this repo tells you nothing about the runtime set.**

`running_process::spawn::spawn_daemon_inner` applies the marker to every
daemon-spawn variant, and its own comment names the consumers: *"including the
free functions consumers like **zccache** call directly"*. On a soldr/zccache
machine the set is non-empty, and its members are exactly the processes that
must never be reaped.

Known setters: **zccache**, **soldr / `soldr-daemon`**.
Known non-setters: **sccache**, **dockerd**, **`FBuildWorker`**, language
servers.

This is why the marker ranks *below* every OS signal: opting in is optional, so
the marker is a bonus, never the only line of defence. See [DD-023] for the
reversal this records.

---

## One host scan per drain, not per notification

The environment pass below is the *most* expensive enumeration, but it is not
the only one. Classifying a `NEW_PROCESS` notification needs the notifying
PID's parent and image name, and the only source for those is
`CreateToolhelp32Snapshot` — a **full enumeration of every process on the
host**, ~20 ms on a 500-process box.

The listener used to take one such enumeration *per completion-port message*,
to look up exactly one PID. Under agent tool-call churn — measured at **~178
process spawns/second**, overwhelmingly short-lived `bash.exe` — that is ~3.6
cores of kernel time in a single session. Worse, the cost is
`O(all processes on the host)` while each concurrent session *adds* to that
count, so N sessions cost more than N times one session (#706).

The rule now:

- **Drain first, then classify.** After the blocking wait returns one message,
  the listener drains everything already queued with a **zero** timeout. The
  drain adds no latency — it collects only what is already there — and it is
  bounded by `MAX_DRAIN_BATCH` so a runaway spawner cannot starve the `stop`
  check.
- **One host enumeration per batch, taken lazily.** `BatchTable` reads the
  process table at most once per drain, and only if some message in the batch
  actually needs it. A batch of exits for already-resolved PIDs never touches
  Toolhelp at all.
- **One reconcile pass per batch.** `batch_needs_reconcile` lives in `model`,
  not in the Win32 listener, so the folding rule is unit-tested on every
  platform. Folding is safe because a pass re-plans the *entire* pending set;
  `ACTIVE_PROCESS_ZERO` is the one message that cancels it, because the reset
  empties the backlog — and a later message in the same batch re-arms it.

Measured on a 48-process burst: **5 host scans instead of ~48**, with 36
messages folded into the largest drain. The win grows with churn, because
heavier load means deeper queues and therefore bigger batches.

`ReapCounters::host_scans` and `peak_batch` make this readable at exit.
`host_scans` is incremented inside `snapshot()` itself, so it counts **every**
enumeration: the batch path, `register_backend`, the exit sweep, and the
quiet-period retry in `retry_unresolved_new_processes`.

That last one matters and is why the counter is not call-site-scoped. It fires
on the 200 ms timeout whenever a PID is still unresolved, so it is the one
recurring host scan the batching does *not* remove. It is self-limiting rather
than permanent — `MAX_METADATA_RETRIES` bounds each PID to 10 quiet periods, so
the set drains in ~2 s — but it is real, and counting only the batch path (as
this first shipped) hid it.

**`host_scans` is therefore not bounded by `ticks`**: one tick can drive both a
retry scan and a batch scan. The bound that holds is against the *message*
count — a burst of N messages must not cost N enumerations — which is what
`reaper_batch_drain_windows` asserts and what `peak_batch` explains.

*Per-PID creation time is deliberately not folded into the batch read.*
`process_identity::start_time_of` is an `OpenProcess` + `GetProcessTimes` —
microseconds, and unlike the shared table it cannot be stale.

---

## One environment pass

Reading a process's environment block means a `ReadProcessMemory` walk of its
PEB — the most expensive enumeration on the machine. Two rules keep it bounded:

- **`process_scan::scan_env` answers both env questions from one snapshot** —
  the originator-tagged set (additive: kill targets) and the declared-daemon set
  (subtractive: spares). Asking `running-process` for them cost two full-host
  passes back to back.
- **`process_scan::DaemonMarkerCache` reads each identity's environment once,
  ever**, over a *bounded candidate set* — the reaper's own job membership,
  never the host.

**Subtractive data may be stale; additive data may not.** A stale spare-list
spares too much, which is harmless. A stale *kill-target enrollment* names a
process that must not be killed. Never build the analogous cache for the
additive side.

**Every map needs an eviction rule at the time it is introduced.** The marker
cache's is `retain_identities`, called every pass against the live candidate set.

### What the pass must exclude

Two filters sit at the front of candidate selection. Both remove rows that
`sysinfo` and PID equality would otherwise accept, and both were added after
the same on-exit report claimed one leaked `__worker` nineteen times over.

- **Threads are not processes.** On Linux `sysinfo` walks `/proc/<tgid>/task/`
  and lists every task alongside its group leader; a thread reads back the
  leader's environment block, so all 19 threads of one tagged process matched
  the originator tag and every reap decision — spare reason included — was
  taken 19 times against the same PID. `classify_scan` skips any row with a
  `thread_kind()`, leaving the group leader to stand for its threads.
- **The originator PID must be plausible, not merely equal.** Linux recycles
  PIDs through `kernel.pid_max` and wraps. A leak tagged `CLUD:<n>` outlives
  the wrap and is eventually met by an unrelated clud that the kernel handed
  the same `<n>`: observed with a `__worker` leaked by an interrupted pytest
  run a day and a half earlier, adopted on exit by a fresh clud that drew PID
  3513633 a second time. The dead-originator sweep already filtered on
  `parent_is_plausible` (the claimed parent must not have started *after* its
  claimed child); `orphan_reaper::select_own_descendants` now applies the same
  guard to the on-exit path.

---

## Manual reap from the dashboard (#340)

The `clud ui` Processes tab shows a **reap subtree** button on each live row.
It posts `{pid, start_time}` to `POST /proc/reap`
(`daemon/http_proc_reap.rs`). The daemon accepts it only when that exact
identity is a live row of its own cached sample, never the daemon itself, and
then runs `process_tree::kill_tree_filtered_automatic`, which re-checks every
identity at kill time and prunes declared daemons. Why Job Objects still allow
breakaway: [DD-188](../DESIGN_DECISIONS.md#dd-188-job-objects-keep-job_object_limit_breakaway_ok-escapees-are-reaped-by-identity).

## Observability

Reaping is destructive and used to be silent, which is how #651 could be closed
as fixed while the same symptom kept growing. `reap_log.rs` provides:

- `ReapCounters` — ticks, reconcile passes, environment blocks actually read,
  host process-table enumerations and peak drain batch (#706), peak `known` and
  peak backlog, plus the decision census. Printed at exit; `--verbose` adds the
  measurement line.
- `ReapLog` — one JSONL line per **mutation** at
  `~/.clud/state/sessions/<pid>__<epoch>/reap.jsonl`. Buffered, never flushed
  per event (#544 found per-op synchronous JSONL flushes to be an idle-CPU cost
  of their own), and nothing at all is written for a pass that changed nothing.
  Every recorded decision includes timestamp, PID, executable, immediate parent,
  rule/reason, action, and phase so a later incident can be attributed without
  inferring process lineage from a partial UI state (#773). Unit tests and
  processes marked `CLUD_INTEGRATION_TESTS=1` write the same layout beneath
  `~/.clud/state/test-sessions/`, isolated from production diagnostics.

**Metadata misses go to the session log, never the shared one.** A job
notification for a PID Toolhelp could not resolve before it exited is *expected*
under process churn — the path fails closed and spares it. Enumerating each one
into `daemon-events.jsonl` synchronously ran at ~300 writes/min, made them 98.8%
of that log, and rotated every other producer's events (including the daemon's
own orphan-sweep status) out before anyone could read them (#689). Per-PID lines
now go to this session's buffered `reap.jsonl`; `ReapCounters::metadata_misses`
carries the total to the exit summary. The diagnostic is relocated and
summarized, not deleted.

Two reconciliation identities, checkable from the summary:

```
shell_exits_observed == finalized + abandoned + still_pending_at_exit
decisions_emitted    == reaped + spared
```

They stay separate, and `tracked` belongs to neither, because the populations
are disjoint: most spawned processes never become reap candidates; abandoned
identities are *triggers* while kill targets are their *descendants*; and a kill
takes a whole subtree without emitting a decision per descendant. `metadata_misses`
is outside both for the same reason: an unresolvable process never became a
candidate, so it emitted no decision.

When reading the log, check the **reason**, not only the outcome. A daemon
spared as `outside_job_object` or `listening_endpoint` was spared by a signal
the code evaluated. A daemon that never appears at all was spared by accident —
most likely because the plan produced nothing — and that will regress the next
time the planner changes.

---

## Where a reaper change gets tested

> **Prefer unit tests. They are faster and they run on every platform.** An
> integration test is justified only when the behaviour cannot be expressed
> against injected process facts — in practice, only when a real Job Object or
> real process detachment is the thing under test.

This is only possible because reap decisions take OS facts as **data**
([DD-024]). A change that calls Win32 inline silently forces itself into the
slow lane and out of cross-platform coverage.

- **Tier 1 — unit** (`job_orphan_reaper`'s `lifecycle_tests`, `orphan_reaper`'s
  and `reaper_facts`' test modules): the decision table. Synthetic process graphs
  plus a `FactsSnapshot` fixture. Assert **spare + reason**, never just the
  outcome. Every new daemon shape, precedence rule, negative case and
  state-bounding rule goes here.
- **Tier 2 — integration**: a real Job Object, a real breakaway, a real detached
  listener. **Budget: ≤5 tests per file.** Anything expressible in Tier 1 must be
  in Tier 1. Uses `testbins/daemon-stub`, never a real sccache/docker install.
  - `reaper_daemon_survival_windows.rs`, `tool_shell_lifecycle_windows.rs` — the
    job reaper's tool-shell-exit path.
  - `reaper_orphan_sweep_survival.rs` — the cross-platform reaper's `clud slay` /
    daemon-sweep path and its on-exit scan (#688). It drives
    `reap_orphans_filtered` with an admission closure narrowed to its own stub
    PIDs, so a full-host sweep on a developer's machine cannot reap somebody
    else's work; the hook runs before `report_and_reap`, so the code under test
    is exactly what `clud slay` executes.
- **Tier 3 — opt-in** (`bench/README.md` runbook): checking the stub's signal
  shapes against genuinely installed daemons on a developer box. Never gating.

Daemon-stub tests need raw `std::process::Command` and must be added to
`ci/banned_imports.py`'s exempt set with a rationale: `NativeProcess` would
attach its own containment *and set the very marker whose absence is under test*.

[DD-021]: ../DESIGN_DECISIONS.md#dd-021-automatic-windows-tool-cleanup-requires-positive-lifecycle-roles
[DD-023]: ../DESIGN_DECISIONS.md#dd-023-the-daemon-spare-list-is-not-deleted-even-though-clud-never-sets-the-marker-itself
[DD-024]: ../DESIGN_DECISIONS.md#dd-024-reap-decisions-take-injected-process-facts-rather-than-calling-win32-inline

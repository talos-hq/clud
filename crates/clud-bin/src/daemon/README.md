# daemon/

`checkout_claims.rs` owns live checkout presence and exclusive mutation claims
(see [checkout claims](../../../../docs/architecture/checkout-claims.md)).
`session_flags.rs` owns the per-session flag dictionary hooks read and write
(see [session controls](../../../../docs/architecture/session-controls.md)).

Provider/harness metadata travels additively inside each worker's
`LaunchPlan`; repeat commands pin both resolved choices. See
[docs/architecture/codex-via-claude.md](../../../../docs/architecture/codex-via-claude.md)
for the cross-route ownership boundary and
[docs/architecture/launch-targets.md](../../../../docs/architecture/launch-targets.md)
for field-level resolution.
Normalized model/effort/context fields and unified-mode repeat reconstruction
are owned by
[docs/architecture/provider-selection.md](../../../../docs/architecture/provider-selection.md).
Daemon worker environment layering and refresh are owned by
[docs/architecture/daemon-environment.md](../../../../docs/architecture/daemon-environment.md).
An unsafe session's marker is client-owned and removed from the daemon
baseline before the next client merges; see
[unsafe-mode.md](../../../../docs/architecture/unsafe-mode.md).

Always-on background service for every `clud` invocation (issue #135). One long-lived daemon process per user owns two distinct concerns, served over the RPC lanes below:

1. **Session manager** — spawns per-session worker subprocesses for `clud --detach`, `clud attach`, `clud list`, `clud kill`, `clud logs`, and `clud loop --repeat`. Each worker owns one backend (`claude` or `codex`) running under a PTY or a captured subprocess, persists snapshots + an append-only log to disk, and brokers attach/detach from interactive clients.
2. **GC service** — single-owner of `~/.clud/data.redb`. All `clud gc *` IPC ops route to a dedicated registry-worker thread (`gc_service.rs`); the worker is the sole reader/writer of the redb file, eliminating multi-process locking races (issue #138). The background repo-worktree probe also refreshes the [read-only target-size report](../../../../docs/architecture/gc-and-registry.md#cached-rust-target-sizes-1687); `gc list --targets` reads that snapshot without IPC.

Foreground interactive launches still use the direct runner by default — the daemon hosts the centralized PTY path only when explicitly opted in (`--detach`, `--detachable`, `--experimental-daemon-centralized`, repeat jobs). See [docs/architecture/daemon-ipc.md](../../../../docs/architecture/daemon-ipc.md) for the wire protocol, attach flow, snapshot/log persistence, and failure modes. This README is the per-file inventory.

Internal helper subcommands `__daemon` and `__worker` re-enter the same binary in their respective roles.

## Listeners

Three, not one. A reader who assumes "the loopback TCP listener" is the whole
story will reach the wrong conclusion about what the daemon can already do
(#692).

| Listener | Transport | Purpose |
|---|---|---|
| **Broker frame lane** | named pipe (Windows) / Unix socket (elsewhere) | the **default** path for every `DaemonRequest`. `rp_broker/` |
| **Loopback TCP** | `127.0.0.1:<port>` in `daemon.json` | fallback for the same requests, and the worker-attach streams |
| **Dashboard HTTP** | `127.0.0.1:<dashboard_port>` in `daemon.json` | the web dashboard; `http.rs` |

`RUNNING_PROCESS_DISABLE=1` disables the frame lane, and every caller falls back
to TCP. The frame lane multiplexes many frames over one connection; the TCP lane
is one request per connection. See
[daemon-ipc.md](../../../../docs/architecture/daemon-ipc.md) for which is which
and why it matters.

## Files

- `mod.rs` — module root. Re-exports the public surface: `ensure_daemon`, `default_state_dir`, `ENV_ALLOW_DAEMON_SPAWN`, `ListRow`, `gc_client_{list,purge,reconcile,insert}` plus the existing `experimental_enabled` / `handle_special_command` / `run_centralized_session`.
- `entry.rs` — public dispatch: feature-flag check, routing for `attach`/`kill`/`list`/`logs`/`daemon stop`/`daemon restart`/`daemon orphan-status`/`__daemon`/`__worker`, and the main `Create` request for normal sessions. `daemon orphan-status` reads the latest `orphan_sweep_finished` event from `daemon-events.jsonl` and exits non-zero if no sweep ran within 2× the interval (#465).
- `types.rs` — shared structs, enums, env-var keys, and constants (`SessionSnapshot`, `WorkerLaunchSpec`, `DaemonRequest`/`Response`, `GcOp`/`GcReply`, `ListRow`, `WorkerClientMessage`/`ServerMessage`, `SessionRuntime`, `RawTerminalGuard`, `ENV_ALLOW_DAEMON_SPAWN`).
- `api_sessions.rs` — #1041 durable logical API-session storage: immutable canonical CWDs, provider identities, turn generations, bounded cursor events and idempotency entries, advisory-lock mutation, restart recovery, and corrupt-record quarantine. It owns no worker process or HTTP route; unlike `SessionSnapshot`, an idle record is resumable.
- `paths.rs` — filesystem layout helpers under the daemon state dir (`default_state_dir` → `~/.clud/state`, `daemon.json`, `daemon.lock` bringup serialization, `sessions/`, `specs/`, `logs/`).
- `rp_broker/` — the **default** RPC lane, over `running-process`'s broker v1 frame protocol. `endpoint.rs` resolves the named pipe / Unix socket and owns the `daemon-identity.json` sidecar (which lets a client skip the Hello handshake); `frame_lane.rs` is the daemon half and multiplexes frames over one connection; `mod.rs` is the client half and sends exactly one request per adopted session. `RUNNING_PROCESS_DISABLE=1` turns the whole lane off.
- `http.rs` — the dashboard's loopback HTTP listener, on its own `dashboard_port`.
  It also serves the `gh` read broker's `/gh/read` (one thread per request)
  and `/gh/invalidate`; see [gh-read-broker.md](../../../../docs/architecture/gh-read-broker.md).
- `cpu_alert_publish.rs` — #547: samples the daemon's own CPU every 2 s and
  writes `<state_dir>/metrics.json` **only when the alert-relevant state
  changes** (crosses 70%, or moves ≥10 pts while above it). Replaces one
  `DaemonRequest::Metrics` round-trip per open terminal every 2 s with one
  sampler and a client-side `stat`. An idle daemon *re-writes* nothing;
  `should_publish` is a pure function so that property is unit-tested rather
  than benchmarked. Clients read it in `console_title.rs::classify_snapshot`,
  falling back to the `Metrics` RPC at 15 s when the file is absent (an older
  daemon). **One `BASELINE_CPU_PCT` snapshot is written at publisher startup
  (#706)** so the file always exists: `should_publish(None, x)` is false below
  the threshold, and nothing else creates the file, so on a healthy box that
  never spikes it was never created — `classify_snapshot` returned `Absent`
  forever and the 15 s "older daemon" fallback was the *steady state* on the
  current daemon.
- `client.rs` — client-side daemon RPC: `ensure_daemon` (idempotent fs4-locked auto-spawn; the detach itself is `spawn_detached_daemon`, running-process's daemon spawn with `EnvironmentPolicy::Inherit` and the originator tag removed, #1186), forward-only daemon replacement (an older client refuses a newer daemon with a yellow exit-1 error; on Linux a same-version daemon whose executable `/proc/<pid>/exe` reports ` (deleted)` is replaced too, because a reinstall leaves it running a build no client can be, #1841), foreground-client lease acquisition/release, `send_daemon_request`, `request_session_termination`, `gc_client_*` IPC wrappers for the four `clud gc` ops plus the creation-ledger `gc_client_insert_created` / `gc_client_query_created` (#1666; the query never spawns a daemon), stale-state cleanup.
- `client_compat.rs` — version-skew helpers for client bringup and old-daemon connection signatures.
- `client_leases.rs` — daemon-owned foreground-client lease registry keyed by PID plus process start time. Acquires and releases are idempotent, and one batched PID refresh prunes clients that exit without releasing.
- `runtime_config.rs` — typed, startup-only daemon policy. `CLUD_DAEMON_TEST_MODE=1` enables the scanner-free self-expiring integration profile; production ignores test lifetime overrides.
- `server.rs` — daemon-process entry (#2142: the accept loop also stops on the process `interrupted` flag, which SIGTERM/SIGHUP/SIGQUIT only flip, and when `daemon.json` names another pid, so a superseded daemon releases `data.redb` and leaves its successor's `daemon.json` alone): binds the frame lane, the loopback TCP listener and the dashboard listener, applies `DaemonRuntimeConfig`, spawns the GC registry worker, checks a supplied daemon process identity on explicit shutdown, serves all eleven `DaemonRequest` variants, spawns worker subprocesses, reaps them. Also owns the periodic **dead-originator orphan sweep** (`spawn_orphan_sweeper` → `run_orphan_sweep` → `orphan_reaper::reap_orphans_filtered_sparing`, issue #465): a `clud-orphan-sweep` thread that reaps `RUNNING_PROCESS_ORIGINATOR=CLUD:<dead-pid>` subtrees past a first-seen grace window, sparing any originator in the handover registry, and logs `orphan_sweep_{started,finished}` events. Both knobs live in `~/.clud/settings.json` and are read once at bringup:

  | Setting | Default | Floor | Meaning |
  | --- | --- | --- | --- |
  | `daemon.orphan_sweep.interval_ms` | 60 000 | 5 000 | period between sweeps. The floor exists because each sweep reads `environ` for every process on the host. |
  | `daemon.orphan_sweep.grace_ms` | 10 000 | 1 000 | how long a freshly observed orphan is left alone. Below the floor the sweep starts racing the on-exit `Drop` guard that owns the same cleanup. |

  `CLUD_ORPHAN_SWEEP_INTERVAL_MS` still overrides the interval and **outranks** the settings file — it is the break-glass knob for an incident on a box whose config you would rather not edit. A garbage env value falls through to settings rather than discarding the configured value.
- `gc_service/source_guard.rs` — fresh host-process and Docker-bind protection before automatic source reclamation; missing facts spare the source (#1790). See [source reclamation safety](../../../../docs/architecture/gc-and-registry.md#source-reclamation-safety-and-the-1790-incident).
- `gc_service.rs` — single-owner registry worker thread (issue #135): opens `~/.clud/data.redb` once, serializes every `gc.*` op through an `mpsc::Receiver<GcRequestMsg>`, including the creation-ledger `InsertCreated` / `QueryCreated` ops and the tick's 72h ledger expiry ([gc-and-registry.md](../../../../docs/architecture/gc-and-registry.md), #1666). Replaces the standalone `gc_daemon` process that shipped in Phase 1. The periodic tick also fans the filesystem sweeps (`uv_cache_sweep`, `session_tmp_sweep`, `session_state_sweep`, `target_sweep`) onto a detached `clud-gc-sweep` thread, prioritized by disk pressure then system CPU load (`spawn_maintenance_sweeps` / `maintenance_action`), and (issue #549) runs a synchronous `sessions::reconcile_session_records` pass that tombstones crash-leftover session records. See [gc-and-registry.md → Filesystem sweeps](../../../../docs/architecture/gc-and-registry.md#filesystem-sweeps-non-registry).
- `gc_service/repo_worktree.rs` + `gc_service/repo_worktree_probe.rs` — issue #1591: squash-aware verdict (pure, decision-table tested) over the worktrees of visited repos, probed on a `clud-gc-repo-worktree-probe` thread and surfaced as `repo-worktree` rows in `clud gc list`. `gc_service/repo_worktree_reclaim.rs` (pure selection, re-verification, ref-deletion and process-cwd decisions) + `gc_service/repo_worktree_reclaim_exec.rs` (purge-pool executor) remove re-verified `reclaimable` worktrees and their local branches on the next tick (#1603), one reclaim per repo at a time (#1632, [gc-and-registry.md → one reclaim per repo](../../../../docs/architecture/gc-and-registry.md#repo-worktrees-squash-aware-verdict-1591-and-reclaim-1603)). Children of `~/.clud/worktrees` also seed the probe, and only they qualify for the `abandoned-empty` / `grace` rule; an unclaimed empty child is judged by `reserved_dir_verdict` and removed with `remove_dir` as `reserved-unused` (#1485, #1486, [gc-and-registry.md → worktree root](../../../../docs/architecture/gc-and-registry.md#the-worktree-root-cludworktrees-1485)). See [gc-and-registry.md → Repo worktrees](../../../../docs/architecture/gc-and-registry.md#repo-worktrees-squash-aware-verdict-1591-and-reclaim-1603). `gc_service/repo_worktree_cli.rs` is the one bridge `--clean-worktrees` uses to share that probe, verdict and executor (#1606, [gc-and-registry.md → `--clean-worktrees`](../../../../docs/architecture/gc-and-registry.md#--clean-worktrees)).
- `gc_service/shim_hints.rs`: issue #2103. Tails the `git`/`gh` shim log from a saved offset and resolves each call's cwd and directory arguments to their main checkout, using filesystem reads only. Repos found this way join the repo-worktree probe and are recorded with `Registry::note_repo`. See [gc-and-registry.md → Repo worktrees](../../../../docs/architecture/gc-and-registry.md#repo-worktrees-squash-aware-verdict-1591-and-reclaim-1603).
- `uv_cache_sweep.rs` — issue #423: daily age pass over `~/.clud/cache/uv/environments-v2`, then the `cache.max_bytes` cap (#1691), which runs `uv cache clean` when over the cap and no uv is running (`crate::gc::uv_cache::enforce_cap`, [DD-145](../../../../docs/DESIGN_DECISIONS.md#dd-145-the-uv-cache-is-capped-by-uv-cache-clean-never-by-deleting-buckets)). Between the two, `crate::gc::uv_cache_repair` invalidates live wheel pointers to gutted archives under uv's locks (#1711, [DD-148](../../../../docs/DESIGN_DECISIONS.md#dd-148-a-corrupt-uv-wheel-entry-is-invalidated-under-uvs-entry-lock-not-cleaned)).
- `startup_phases.rs` — #2333: `StartupPhases::mark` emits `daemon_startup_phase` events (phase + `elapsed_ms`) and `fail` records `daemon_start_failed` plus `<state_dir>/daemon.stderr.log` for `run_daemon`'s early exits; `install_panic_hook` (once, from the `InternalDaemon` entry arm only) chains a panic hook that appends panics (message, location, backtrace) to the same log (#2386); `redirect_stderr_to_log` also points the daemon's stderr at that log (via `platform::stdio`) so stray non-panic output is kept. Rotation runs at startup before the redirect. See [daemon-ipc.md](../../../../docs/architecture/daemon-ipc.md).
- `session_tmp_sweep.rs` — issues #509/#1260: 72h retention under `~/.clud/tmp`. A new sweep starts at most every 6h, then its background worker keeps advancing and checkpointing pending work without waiting for another hourly GC tick; deferred failures resume later from `state/session-tmp-sweep.work.json`. Each checkpoint reports scan, deletion, cursor, and retry progress to `daemon-events.jsonl` (see `crate::gc::session_tmp`).
- `session_state_sweep.rs` — issue #1014: 6h-throttled sweep of the forensic session tree `~/.clud/state/sessions/` (one `<pid>__<epoch>/` directory per launch, holding `reap.jsonl`, `reap-health.json`, and since #1011 a `bridge.jsonl`). Nothing aged that tree out, so it grew one entry per session forever — the sibling `state/launches/` has been bounded by `MAX_RECORDS` since #998. Two retention windows, because the point of #998/#1011 is that a failure trail survives to be read: 48h for a directory whose bridge log holds only ambient records, 30d for one holding anything else. A live session's directory is never touched (liveness is injected from here so `crate::gc::session_state` stays free of process introspection). Policy lives in `crate::gc::session_state`; this file owns only the throttle and the liveness predicate.
- `trash_cap.rs` — issue #1672: `trash.max_bytes` (default 20 GiB, `0` off) evicts the oldest `~/.clud/trash` entries past the cap, never one younger than an hour; pure `plan_eviction` over injected facts. Contract in [gc-and-registry.md](../../../../docs/architecture/gc-and-registry.md#filesystem-sweeps-non-registry).
- `target_sweep.rs` — issue #510: opt-in 24h-throttled sweep of stale Rust `target/` dirs under `CLUD_GC_TARGET_ROOTS` (see `crate::gc::target_sweep`).
- `worker.rs` — worker-process entry: starts the backend (subprocess or PTY), serves attach connections, runs the repeat-job loop. The PTY read thread answers `ESC[6n` and the other terminal probes (`terminal_queries::TerminalQueryScanner`, shared with the local pump) when no client is attached, so a `--detach` ConPTY session does not hang at startup (#1366, #1702).
- `worker_shared.rs` — per-worker shared state: snapshot, in-memory backlog, optional `TerminalCapture` for PTY attach-replay, log file rotation, single-client attach gate.
- `attach.rs` — interactive client-side attach loop: handshake, raw-terminal keyboard forwarding, Ctrl-C → background-prompt flow, exit-code propagation. Holds the kitty keyboard frame (`session::KeyboardEnhancementGuard`) across the output relay (#1363) and strips the child's OSC 0/2 titles in it (#1372). The input loop (`pump_remote_input`, behind the `RemoteAttachIo` seam) ends with the session's exit code once the reader records it (#1448); see [daemon-ipc.md attach flow](../../../../docs/architecture/daemon-ipc.md#attach-flow).
- `commands.rs` — implementations of `clud kill`, `clud list`, `clud logs` (including pm2-style tail/follow with rotation handling).
- `sessions.rs` — snapshot discovery + filtering: `resolve_session_id` (exact/name/prefix), `most_recent_session[_any]`, `list_background_sessions`, `list_attachable_sessions`. Also `reconcile_session_records` (#549): retires crash-leftover records (worker **and** root dead, no clean exit, past a 10 min grace) by renaming `<id>.json` → `<id>.json.tombstone` (invisible to every `.json`-filtered lister) and deletes tombstones older than 7 days. Never terminates a process.
- `attach_input.rs` — raw input for the interactive attach (#1355): the byte source (Windows `console_input`, POSIX polled stdin) and `RemoteInputFilter`, which forwards every byte except Ctrl+C, so terminal replies such as the answer to ConPTY's `ESC[6n` reach the worker, and expands a byte-stream source's Ctrl+V to a clipboard image path (#1373). Contract: [daemon-ipc.md attach flow](../../../../docs/architecture/daemon-ipc.md#attach-flow).
- `io_helpers.rs` — JSON read/write over TCP + atomic file writes, session-id generator, terminal-size probe, `--backlog-size` / `CLUD_BACKLOG_BYTES` parsing, and the shared child-environment policy merge.
- `login_env.rs` — #933's refreshable login baseline: bounded POSIX login-shell capture, Windows machine/user registry materialization, admission snapshots, and the five-minute daemon refresh backstop. Full contract: [daemon environment](../../../../docs/architecture/daemon-environment.md).
- `wire_prost/` - prost v1 foundation for the daemon wire: generated `clud.v1` types, CLUD/CLJS payload protocol discriminators, encode/decode helpers, JSON-compatibility tests, the default prost daemon RPC path, and the `CLUD_DAEMON_WIRE=json` legacy fallback.
- `handover_registry.rs` — persisted `~/.clud/state/handover-registry.json` (issue #465): the set of originator PIDs of intentionally-detached sessions, recorded (with the launching daemon's PID) when a detached/background session is created so a *successor* daemon's sweep spares the still-live session across a restart. Purely subtractive on the reap path — `load`/`save`/`register`/`prune`; a stale entry can only leave an orphan un-reaped, never cause a wrong kill.
- `headless_adapter.rs` — #1039's typed CLI-only headless-turn adapter: sends initial/resumed Claude and Codex turns through the canonical command builder and parses their JSONL provider identities. It deliberately owns neither worker draining nor durable session state; later lifecycle slices consume its plan and normalized events.
- `process_utils.rs` — `pid_is_alive`, `signal_process_tree`, `descendant_pids` via `sysinfo`, plus their identity-guarded twins `identity_is_alive` / `signal_process_tree_as` (issue #558). Terminated processes, including unreaped Unix zombies, are not live (#1993). Anything working from a PID that came off disk uses the guarded pair; `SessionSnapshot::{worker,root,daemon}_identity()` and `DaemonInfo::identity()` build the argument. See `crates/clud-bin/src/process_identity.rs`.
- `api_turn_controller.rs` — #1042 captured subprocess controller for one logical-session generation: persisted-CWD validation, invisible Windows launch, independent JSONL drain, raw-log rotation, provider-ID capture, and drain-before-completion sealing. It owns no HTTP or interrupt/kill policy.
- `api_session_lifecycle.rs` — #1043 serialized logical-session lifecycle: one active API generation, atomic durable idempotency claim/replay/conflict, provider-ID-gated resume, graceful interrupt with bounded forced fallback, replace sequencing, and terminal kill. It does not alter interactive attach Ctrl-C or legacy daemon interrupt RPC behavior.
- `board.rs` — in-memory agent bulletin board behind `/v1/board/messages`; see [agent-board.md](../../../../docs/architecture/agent-board.md).
- `api_session_http.rs` — #1044 bearer-only `/v1/sessions` routing: typed create/list/get/events/turn/interrupt/delete DTOs, idempotency replay, stable errors, and headless-plan reconstruction from durable settings and the immutable canonical CWD. It is separate from dashboard capability auth and worker attach routes.
- `api_session_inject.rs` — operator injection: durable `injections[]` on the session record, `POST /v1/sessions/{id}/inject`, delivery now / at the next turn boundary (drained by the lifecycle observer) / via `interrupt`. `inject_client.rs` is the `clud inject` CLI. See [session-inject.md](../../../../docs/architecture/session-inject.md).

- `proc_sampler.rs` - daemon-owned process sampler for `clud top`: keeps one persistent `sysinfo::System`, refreshes CPU/RSS/parent data on the hot tick, refreshes `RUNNING_PROCESS_ORIGINATOR` tags on a slower cadence, and serves cached `ProcTreeSnapshot` replies over daemon IPC.
  `decide_cadence` parks the tick at 30 s when nobody has asked for a snapshot
  in the grace window *and* no sessions are live (#548), and
  `originator_scan_due` additionally skips the **host environment scan** on a
  parked tick. That gate is the point: the parked interval and the scan
  interval are both 30 s, so before it a fully idle daemon still paid a
  `ReadProcessMemory` walk of every process's PEB on *every* parked tick —
  parking had cut the sampling but not the expensive half. The annotation that
  scan produces has no reader while parked, and a consumer's arrival wakes the
  sleep early so the next tick is `Active` and rescans; `clud top` renders
  `snapshot.refresh_age()`, so a first stale reply is visible rather than
  silent. **Still outstanding on #548:** the periodic orphan sweep
  (`server.rs`) does its own independent `process_scan::scan_env("CLUD")`, so
  the two full-host env passes are not yet unified.
- `top.rs` - `clud top` snapshot filtering, sorting, JSON preparation, and text tree/flat rendering.

## `clud top --json` schema

`DaemonRequest::ProcSnapshot { include_dead_since_ms }` returns the cached sampler payload below. The CLI applies `--originator`, `--sort`, `--flat`/`--tree`, and `--limit` to the same shape before printing JSON.

```json
{
  "schema_version": 1,
  "sampled_at_ms": 1234567890,
  "sample_age_ms": 42,
  "sampler_pid": 71584,
  "interval_ms": 2000,
  "rows": [
    {
      "pid": 73000,
      "ppid": 71584,
      "originator": "CLUD:71584",
      "originator_pid": 71584,
      "session_id": "sess-example",
      "session_name": "build",
      "cpu_pct": 12.5,
      "cpu_ewma_pct": 8.1,
      "rss_bytes": 104857600,
      "age_secs": 30,
      "command": "codex exec",
      "depth": 1,
      "tier": "hot",
      "live": true
    }
  ],
  "summary": {
    "process_count": 1,
    "originator_count": 1,
    "total_cpu_pct": 12.5,
    "total_rss_bytes": 104857600
  }
}
```

Dead rows are omitted by default. Passing `--since <duration>` sets `include_dead_since_ms`; matching dead rows have `"live": false`, `"tier": "frozen"`, and an `exited_at_ms` field.

## Key items

- `pub fn experimental_enabled(&Args) -> bool` — `entry.rs:60`
- `pub fn handle_special_command(&Args, &AtomicBool) -> Option<i32>` — `entry.rs:526`
- `pub fn run_centralized_session(&Args, &LaunchPlan, &AtomicBool) -> i32` — `entry.rs:699`
- `enum DaemonRequest` — **11 variants**: `Create`, `Session`, `ListLiveCwds`, `Terminate`,
  `Interrupt`, `AdoptKill`, `Gc`, `Shutdown`, `ReapOrphans`, `Metrics`, `ProcSnapshot`
  — `types.rs:216`. Full table with payloads and replies in
  [`daemon-ipc.md`](../../../../docs/architecture/daemon-ipc.md).
- `enum DaemonResponse` — the twelve replies plus `Error` — `types.rs:276`
- `enum WorkerClientMessage { Attach, Input, Resize, Interrupt }` — `types.rs:557`
- `enum WorkerServerMessage { Attached, Output, Exited, Error }` — `types.rs:582`
- `struct SessionSnapshot` — on-disk + wire session metadata — `types.rs:80`
- `struct WorkerLaunchSpec` — daemon→worker launch contract — `types.rs:185`
- `enum SessionRuntime { Subprocess, Pty }` — runtime handle abstraction — `types.rs:590`
- `enum SessionKind { Subprocess, Pty }` — `types.rs:49`
- `const ENV_FEATURE_FLAG = "CLUD_EXPERIMENTAL_DAEMON"` — `types.rs:20`
- `const ENV_STATE_DIR = "CLUD_DAEMON_STATE_DIR"` — `types.rs:21`
- `const DEFAULT_BACKLOG_LIMIT_BYTES = 256 KiB` — `types.rs:26`
- `const LOG_ROTATE_BYTES = 10 MiB` — `types.rs:34`
- `fn run_daemon(&Path, &AtomicBool) -> i32` — `server.rs`
- `fn run_worker(&Path, &str, u32, &Path) -> i32` — `worker.rs:32`
- `fn ensure_daemon(&Path) -> io::Result<()>` — `client.rs:43`
- `fn send_daemon_request(&Path, &DaemonRequest)` — `client.rs:168`. Tries the
  `rp_broker` frame lane (named pipe / Unix socket) **first**, falling back to
  loopback TCP on any miss.
- `fn run_attach(&str, &Path, &AtomicBool) -> i32` — `attach.rs:172`
- `fn run_kill / run_list / run_logs` — `commands.rs:15`, `commands.rs:90`, `commands.rs:167`
- `fn resolve_session_id(&Path, &str)` — `sessions.rs:20`
- `struct WorkerShared` (+ `attach_client`, `push_output`, `broadcast_exit`, `evict_dead_client`, log rotation) — `worker_shared.rs:95`
- `struct RemoteInputFilter` (`new(expand_ctrl_v)`, `process`, `flush_pending`) — `attach_input.rs:49`; `RawInput::expands_ctrl_v` picks the flag
- `fn resolve_backlog_bytes(Option<&str>) -> Option<usize>` — `io_helpers.rs:77`
- `fn signal_process_tree(u32, Signal)` — `process_utils.rs:67`;
  `fn signal_process_tree_as(&ProcessIdentity, Signal)` — `process_utils.rs:57`

## Used by

- `crates/clud-bin/src/main.rs` — sole external consumer; calls `experimental_enabled`, `handle_special_command`, and `run_centralized_session`.
- `crates/clud-bin/src/process_tree.rs` — doc-only cross-reference to `signal_process_tree`.
- Re-enters itself via the hidden `__daemon` / `__worker` subcommands defined in `crates/clud-bin/src/args.rs`.

Session rm activation and hook identity checks: [rm protection](../../../../docs/architecture/rm-protection.md).

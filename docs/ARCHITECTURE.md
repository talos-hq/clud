# clud Architecture

Index of subsystem architecture docs. Each file is self-contained for one
cross-cutting concept; per-directory READMEs link here instead of
re-explaining.

## Subsystem Docs

| Document | Lines | What it covers |
|---|---|---|
| [architecture/secret-prompt.md](architecture/secret-prompt.md) | ~60 | Masked native credential input, direct sinks, caller-checked askpass, output secrecy and cancellation |
| [architecture/loop-subsystem.md](architecture/loop-subsystem.md) | ~250 | `clud loop`: task resolution, plan synthesis, iteration run, DONE/BLOCKED marker contract, artifact rollover, repeat scheduling |
| [architecture/workflow.md](architecture/workflow.md) | ~60 | `clud workflow`: Anthropic-format scripts in embedded QuickJS, `agent()` batches as concurrent `clud -p` children, ASCII board, `WORKFLOW_DONE.md` |
| [architecture/testing-tiers.md](architecture/testing-tiers.md) | ~110 | The three test tiers; tier 3 runs the real Claude Code on `mock-agent serve` (scripted model), with the `tests/harness/` fixture and its pitfalls |
| [architecture/grind.md](architecture/grind.md) | ~550 | `clud grind` and the `/grind` skill DAG: meta-issue routing, the plan-only pass and regroup threshold, meta of metas, preflight and the single question round, prework's plan comment, bug and feature stages, feature-branch mode, problem reporting, reconcile, capped `grind-*` agent roles, one-at-a-time integration |
| [architecture/agent-board.md](architecture/agent-board.md) | ~45 | Agent bulletin board: daemon `/v1/board/messages`, the bridge's `board_*` MCP tools, and the system-prompt directive to poll it |
| [architecture/daemon-ipc.md](architecture/daemon-ipc.md) | ~250 | Always-on clud daemon hosting session ops + GC: TCP JSON IPC, daemon/worker re-entry model, snapshot persistence, attach broker |
| [architecture/checkout-claims.md](architecture/checkout-claims.md) | ~35 | Live checkout presence, exclusive mutation claims, CLI and hook behavior, restart grace |
| [architecture/daemon-environment.md](architecture/daemon-environment.md) | ~100 | Daemon session environment: login-baseline materialization, client overlay, refresh, worker snapshot compatibility |
| [architecture/api-session-storage.md](architecture/api-session-storage.md) | ~160 | Durable API logical sessions above provider turns: immutable CWD, bounded events/idempotency, atomic persistence, restart recovery |
| [architecture/session-inject.md](architecture/session-inject.md) | ~140 | Steering a running daemon session: `session_inject` MCP tool, `clud inject`, queued-at-boundary vs `interrupt`, per-harness support matrix, `{accepted, state, seq}` durability and `client_msg_id` idempotency |
| [architecture/session-lifecycle.md](architecture/session-lifecycle.md) | ~300 | PTY session pump, console mode setup, OSC title keeper, capture for attach, drag-drop and voice injection points |
| [architecture/terminal-pty.md](architecture/terminal-pty.md) | ~220 | Cross-platform terminal/PTY contract: input and output data flow, the three invariants (no partial escape sequence in a PTY write, resumable stream scanners, terminal restore on exit), test tiers and CI labels; links the per-platform agent skills |
| [architecture/session-history.md](architecture/session-history.md) | ~110 | `clud -c` cwd session picker, per-cwd index kept by lifecycle hooks, native/forked/portable resume decision table, recovery payload (#922) |
| [architecture/skill-system.md](architecture/skill-system.md) | ~200 | Skill bundling (`include_str!`), the single `skills.rs` installer over `assets/skills/`, the four-state install contract, selected-backend global setup |
| [architecture/launch-setup.md](architecture/launch-setup.md) | ~70 | Session-only vs global launch setup, persistent setup actions, selected-backend gating |
| [architecture/gc-and-registry.md](architecture/gc-and-registry.md) | ~250 | always-on `clud __daemon` single-owner redb model, session cap registry, worktree scanner, GC subcommands |
| [architecture/platform-boundary.md](architecture/platform-boundary.md) | ~80 | The `platform/` boundary: the only place a host is named, the selector and concrete trees, stubs for one-host concepts, and the no-exceptions `enforce_platform_boundary` lint |
| [architecture/windows-quirks.md](architecture/windows-quirks.md) | ~300 | Windows-only platform code: trampoline, BatBadBat `.cmd` rewrite, console modes, Shift+Enter key translation, `IDropTarget`, `CREATE_NO_WINDOW`, ARM whisper carveout |
| [architecture/route-plan.md](architecture/route-plan.md) | ~140 | `ResolvedRoute`: the one place a launch's backend, upstreams, slots, allowlist, discovery, context, effort, timeouts, scrub list and launch checks are decided; every backend renders it, launch checks run before the backend branch, and request-path features stay gateway-side (#1852) |
| [architecture/launch-plan.md](architecture/launch-plan.md) | ~180 | `LaunchPlan` as the single source of truth: construction, consumers, `--dry-run` JSON |
| [architecture/launch-targets.md](architecture/launch-targets.md) | ~540 | Independent model-provider and harness resolution, sticky settings, foreground bridge lifecycle, compatibility; DeepSeek direct provider (credential trust boundary, preflight, child overlay, no bridge) |
| [architecture/provider-selection.md](architecture/provider-selection.md) | ~120 | #900's compatible launch grammar, direct-vs-unified routing mode, provider-neutral model catalog, modifier normalization, and plan/repeat propagation |
| [architecture/server-settings.md](architecture/server-settings.md) | ~170 | Server-side settings (#1192): the baked-in `server-settings.json` refreshed from `main`, per-section last-known-good merge, bounded-wait refresh and backoff, strict JSON parsing, controls, OpenRouter's context map (#1258) and separate weighted-pricing catalog (#1256) |
| [architecture/unified-gateway.md](architecture/unified-gateway.md) | ~130 | #898/#899's authenticated foreground multiplexer: version gate, discovery, provider routing, route epochs, token counting, credential isolation, and cross-provider effort contract |
| [architecture/standalone-gateway.md](architecture/standalone-gateway.md) | ~40 | Foreground API server: listener, client and provider credentials, discovery, and lifecycle |
| [architecture/secret-redaction.md](architecture/secret-redaction.md) | ~70 | Session-owned reversible gateway tokenization, stream/tool-argument restoration, route policy, count-only status and OpenRouter guardrail advice (#1868) |
| [architecture/codex-via-claude.md](architecture/codex-via-claude.md) | ~120 | Canonical cross-route ownership: resolution, bridge protocol, credentials, security boundary, rollback |
| [architecture/hook-dispatch.md](architecture/hook-dispatch.md) | ~340 | clud running a repo's own hooks instead of the harness: `.clud/hooks.json` declaration, the rooting contract (cwd + `CLUD_PROJECT_DIR` = the declaring repo), the exit-code contract and why it fails open, the per-root firing matrix (parent/child/extern, Tier B source from frontend settings), extern trust via `clud extern trust` (DD-060/DD-061/DD-062), Phase 5's three-level `"auto"` `bash.block_cd` relaxation and the capability-gated `CwdChanged` backstop (DD-063/DD-064), and delivery by compiling settings into CLI args rather than writing config files (#966/#967/#977) |
| [architecture/crash-reports.md](architecture/crash-reports.md) | ~110 | Panic-hook + native crash handler + `clud symbols` verifier: JSON schema, embed-line-tables-everywhere choice, opportunistic-verify model (#374) |
| [architecture/process-reaping.md](architecture/process-reaping.md) | ~200 | The two disjoint reapers, the `(pid, creation_time)` keyspace and its single purge sweep, daemon-sparing by OS signal (marker second, whitelist last), the cooperative-marker caveat, and which test tier a reaper change belongs in |
| [architecture/shell-ast.md](architecture/shell-ast.md) | ~150 | The command scanner's parser (#1962): tree-sitter bash/PowerShell grammars and a cmd.exe splitter producing evaluated invocations, what static evaluation resolves versus marks `Unknown`, each rule's Unknown policy, the worktree location guard, and the recall oracle over real shim-recorded calls |
| [architecture/session-controls.md](architecture/session-controls.md) | ~70 | Switches only the user can turn, enforced on every agent in a session (#1984): prompt → daemon session-flag dictionary, inherited `CLUD_*` launch env, hook enforcement; the worktree consent control |
| [architecture/rm-tools.md](architecture/rm-tools.md) | ~170 | `safe-rm` (#1461): trash, profile roots, audit, hook rewrite, and the child catastrophe floor |
| [architecture/unsafe-mode.md](architecture/unsafe-mode.md) | ~40 | Per-launch `--unsafe` policy and the clud safeguards it disables |
| [architecture/shim-dispatch.md](architecture/shim-dispatch.md) | ~90 | `clud-shim`'s alias registry, single dispatch path, session ABI stamp, and fail-open passthrough outside a session (#1546) |
| [architecture/gh-watch-shim.md](architecture/gh-watch-shim.md) | ~40 | Session `gh` alias, PR-watch translation, and target resolution |
| [architecture/gh-read-broker.md](architecture/gh-read-broker.md) | ~330 | Session `gh api` GETs answered from the daemon's ETag cache (#1743): classification, loopback replay through the real `gh` for byte-identical output, TTL/single-flight/304 revalidation, merged comment and run-list reads that revalidate every page by ETag (deletions, reactions and reruns seen on the next refresh), targeted write invalidation, the redb store and ledger, and every fail-open path |
| [architecture/git-gh-telemetry-shim.md](architecture/git-gh-telemetry-shim.md) | ~90 | Session `git` / `gh` aliases as a telemetry pass-through (#1486): nothing refused, shell-style child handling, the capped JSONL record under `~/.clud/state/logs/shim/`, and how clud's own spawns reach the real binaries |
| [architecture/ci.md](architecture/ci.md) | ~380 | Build-once/run-everywhere CI: per-triple cross-compilation on Linux, test bundles, exec runners with no toolchain, target tiers, release-profile containment |
| [architecture/installer-pages.md](architecture/installer-pages.md) | ~30 | Published release assets to validated v1 Catalog, exact Pages paths, and unauthenticated deployment smoke |
| [architecture/native-installer.md](architecture/native-installer.md) | ~40 | Early installer CLI, selector, exact release choice, consented binary transaction, and pending PATH activation |
| [architecture/model-pages.md](architecture/model-pages.md) | ~35 | Nightly/manual Codex model publication, Pages coexistence, local availability checks, and fallback |
| [architecture/provider-failover.md](architecture/provider-failover.md) | ~200 | Surviving provider exhaustion mid-session (#968): routing OpenRouter through the gateway, the route-health failure taxonomy, the cost-labeled failover ladder, pre-commit request replay versus post-commit degradation, and cooldown recovery |
| [architecture/test-runtime-memory.md](architecture/test-runtime-memory.md) | ~220 | **Design proposal (#405, not yet implemented):** `.clud/`-local test-runtime histogram — append-only JSONL over redb/SQLite and why, raw `(duration, cpu_load)` with query-time normalization, count-based compaction, and the run-all-vs-targeted recommendation policy |
| [architecture/toasts.md](architecture/toasts.md) | ~200 | In-terminal toasts (#1189) and transient CPU HUD (#1359): keyed model and hub, Kitty graphics and alternate-screen text tiers, status line/title fallback, safe PTY composition, opacity/hover, Claude `statusLine` chaining, click-to-dismiss |
| [architecture/web-terminal.md](architecture/web-terminal.md) | ~30 | Desktop web-terminal preference, companion launch/recursion guard, tab PTYs, and wheel packaging |
| [architecture/kitty-windows-parity.md](architecture/kitty-windows-parity.md) | ~75 | #1282 Windows Kitty-style parity matrix, native WezTerm candidate, packaging and GUI verification gates |

## Quick Reference

- **"How do I measure idle daemon and client cost?"** -> [idle CPU benchmark](../bench/idle_cpu/README.md)

- **"How does `clud loop` decide when to stop?"** -> [loop-subsystem.md](architecture/loop-subsystem.md)
- **"What does `clud grind` launch?"** -> [grind.md](architecture/grind.md)
- **"How do `attach` / `list` / `kill` talk to the daemon?"** -> [daemon-ipc.md](architecture/daemon-ipc.md)
- **"Which environment does a daemon-hosted session inherit?"** -> [daemon-environment.md](architecture/daemon-environment.md)
- **"How are API conversation IDs, turns, and polling cursors persisted?"** -> [api-session-storage.md](architecture/api-session-storage.md)
- **"What happens between Ctrl-D and process exit in a PTY session?"** -> [session-lifecycle.md](architecture/session-lifecycle.md)
- **"When does a bundled skill get written into my home?"** -> [skill-system.md](architecture/skill-system.md)
- **"When does clud write agent setup files?"** -> [launch-setup.md](architecture/launch-setup.md)
- **"Why is `~/.clud/data.redb` behind a daemon?"** -> [gc-and-registry.md](architecture/gc-and-registry.md)
- **"Why does CI compile on Linux but test on macOS/Windows?"** -> [ci.md](architecture/ci.md)
- **"Where does the installer version list come from?"** -> [installer-pages.md](architecture/installer-pages.md)
- **"What is clud allowed to kill, and where does my reaper test go?"** -> [process-reaping.md](architecture/process-reaping.md)
- **"Why does Windows do X differently?"** -> [windows-quirks.md](architecture/windows-quirks.md)
- **"Where does the argv that clud runs come from?"** -> [launch-plan.md](architecture/launch-plan.md)
- **"How do provider and harness preferences resolve?"** -> [launch-targets.md](architecture/launch-targets.md)
- **"Which model ID is stable, and how do effort/context reach a worker?"** -> [provider-selection.md](architecture/provider-selection.md)
- **"Where is a launch's routing decided, and what does `--dry-run` show?"** -> [route-plan.md](architecture/route-plan.md)
- **"How do I change a value in installed builds without a release?"** -> [server-settings.md](architecture/server-settings.md)
- **"How does one Claude session switch safely among providers?"** -> [unified-gateway.md](architecture/unified-gateway.md)
- **"How does Codex run through Claude, and how do I roll it back?"** -> [codex-via-claude.md](architecture/codex-via-claude.md)
- **"Why did a project hook stop working after the agent `cd`'d, and who runs hooks now?"** -> [hook-dispatch.md](architecture/hook-dispatch.md)
- **"What happens when clud crashes, and how do I read the report?"** -> [crash-reports.md](architecture/crash-reports.md)
- **"What happens when my provider runs out of quota mid-session?"** -> [provider-failover.md](architecture/provider-failover.md)
- **"Should I run all the tests or just some?"** -> [test-runtime-memory.md](architecture/test-runtime-memory.md) *(design proposal, #405)*
- **"How does `clud --web-term` launch and avoid recursion?"** -> [web-terminal.md](architecture/web-terminal.md)
- **"Which Kitty behaviors must work in the Windows terminal?"** -> [kitty-windows-parity.md](architecture/kitty-windows-parity.md)

See also: [DESIGN_DECISIONS.md](DESIGN_DECISIONS.md) for rationale behind the
choices these subsystems embody.

Release-manager evidence for the optional cross-route is tracked separately in
[release-codex-via-claude.md](release-codex-via-claude.md).

- **rm safety and shim identity** -> [rm-protection.md](architecture/rm-protection.md)
- **"Why does `gh`/`python`/`rm` behave normally outside a clud session?"** -> [shim-dispatch.md](architecture/shim-dispatch.md)
- **"How does an agent delete files, and where do they go?"** -> [rm-tools.md](architecture/rm-tools.md)
- **"Where do in-session `git` / `gh` calls get logged?"** -> [git-gh-telemetry-shim.md](architecture/git-gh-telemetry-shim.md)
- **"How should skills and bundled tools keep output small?"** -> [bounded-output.md](architecture/bounded-output.md)

# src/

The cross-cutting contracts for `clud --codex --harness claude` and
`clud --unified` live in
[`docs/architecture/codex-via-claude.md`](../../../docs/architecture/codex-via-claude.md)
and
[`docs/architecture/unified-gateway.md`](../../../docs/architecture/unified-gateway.md);
this README only maps the source owners.

The per-launch `--unsafe` choice crosses argv, `LaunchPlan`, child environment,
command hooks, and the `rm` alias. See
[unsafe-mode.md](../../../docs/architecture/unsafe-mode.md).

Entry point and source tree for the `clud-bin` Rust binary. The binary launches
a backend agent (`claude` or `codex`) in YOLO mode, optionally through a PTY,
with first-class support for loop iterations, drag-and-drop, voice input, and a
per-user daemon for backgrounded/detachable sessions. `main.rs` does
cross-cutting startup work (trampoline unlock, console title, launch setup
selection, session-cap registration, GC watch registration) and then hands off to
`runner.rs`, which drives the per-iteration subprocess/PTY launch loop for a
single [`LaunchPlan`]. Submodules under `command/`, `daemon/`, `dnd/`, and
`voice/` carry the bulk of the domain logic; the top-level `.rs` files here are
the orchestration glue and standalone utilities consumed by both `main.rs` and
the integration tests.

Credential prompt ownership (`secret_prompt.rs`, `secret_prompt/gui.rs`, and
`secret_prompt/sink.rs`) is documented in
[secret-prompt.md](../../../docs/architecture/secret-prompt.md).

`secret_redaction/` owns session-only reversible request tokenization, stream
restoration and count-only status. See
[secret-redaction.md](../../../docs/architecture/secret-redaction.md).

## Subdirectories

- [command/](command/README.md) - `LaunchPlan` construction: backend argv
  assembly, YOLO/safe injection, `loop`/`up`/`rebase`/`fix`/`do` prompt synthesis,
  `--repeat` schedule parsing, DONE/BLOCKED contract.
- [daemon/](daemon/README.md) - long-lived session manager for `--detach` /
  `attach` / `list` / `kill` / `logs` / `--repeat`: TCP JSON IPC, per-session
  worker subprocesses, snapshot + log persistence, attach broker.
- [dnd/](dnd/README.md) - drag-and-drop into the terminal: cross-platform
  path-string normalizer plus Windows-only `IDropTarget` adapter with
  per-launch-mode injectors.
- [platform/](platform/README.md) - the host-platform boundary (#1953): the
  only place in this crate that names a host. `mod.rs` selects `windows/` or
  `unix/`, and each area (`platform::subprocess`, `platform::rm_tool`, ...)
  has matching items in both trees. See `docs/architecture/platform-boundary.md`.
- [server_settings/](server_settings/README.md) - server-side settings (#1192):
  the `assets/server-settings.json` baked into the binary and refreshed from
  `main`, falling back section by section to the last valid value, plus a strict
  JSON parser. See `docs/architecture/server-settings.md`.
- [self_install/](self_install/README.md) - early installer CLI dispatch and
  selector, native v1 Catalog parsing, host variant resolution, selected
  asset verification, and the consented binary transaction.
- `openrouter_catalog.rs` - #1256's separate bounded catalog consumer: fixed
  GitHub origin, last-known-good daemon-state cache, baked-in fallback, and
  weighted pricing for eligible OpenRouter models, plus
  `catalog_cached_or_embedded()` (no-fetch read for the launch path) and
  `model_by_id()` for #1528's `supports_reasoning_effort` lookup. See
  `docs/architecture/server-settings.md`.
- [session_history/](session_history/README.md) - `clud -c` / `--last` (#922):
  per-cwd index of Claude-harness sessions kept by the hidden `session-hook`,
  the picker, and native/portable resume. See
  `docs/architecture/session-history.md`.
- [toast/](toast/README.md) - in-terminal toasts (#1189) and the transient
  CPU health HUD (#1359): keyed model and hub, the PTY writer-thread compositor
  (Kitty graphics, alternate-screen text cells, title/status-line fallback),
  opacity/hover, Claude `statusLine` chaining, and ordinary-toast dismissal.
  See `docs/architecture/toasts.md`.
- [voice/](voice/README.md) - F3 push-to-talk voice mode: mic capture,
  start/stop cues, `whisper-rs` worker thread, transcript injection into the
  backend PTY.

## Top-Level Modules

`stop_repeat.rs` tells the user once when consecutive Stop-hook continuations end with the same message (#1597, [hook-dispatch.md](../../../docs/architecture/hook-dispatch.md#repeated-stop-hook-continuations)); detection only, never blocks.
`loop_plan_mode.rs` tracks native `/loop` and scheduler activity through
daemon session flags and prevents automatic `EnterPlanMode` while it is
active (#1844). See [hook-dispatch.md](../../../docs/architecture/hook-dispatch.md#loop-scoped-plan-mode).

Entry and orchestration:

- `main.rs` - process entry: launch clock, trampoline unlock, console title
  stamp + keeper, launch setup selection, large-file guard, session-cap
  registration, GC watch registration, dispatch to runner / daemon / hook-health / GC
  subcommands.
- `lib.rs` - library facade so integration tests under `tests/` can link
  against internals; `main.rs` imports through this rather than re-declaring
  `mod ...`.
- `harness_picker.rs` - installed-harness picker and the narrow bare-launch
  DeepSeek-only credential fallback. External Claude/Codex authentication is
  checked through bounded read-only CLI status commands; the DeepSeek vault
  check never replaces the normal launch preflight.
- `runner.rs` - per-iteration subprocess- and PTY-mode runner for a single
  `LaunchPlan`; owns child-env construction, stream-json fallback,
  Ctrl-C-aware teardown, and OLE drag-drop registration wiring. The
  backend-aware `child_env_for_backend` reads
  `~/.clud/settings.json::shell.disable_powershell` and, for Claude, injects
  the two undocumented kill-switch env vars `CLAUDE_CODE_USE_POWERSHELL_TOOL=0`
  + `CLAUDE_CODE_GIT_BASH_PATH` (resolved via
  [`shell/`](shell/README.md)) — see issue #447.
- `webterm.rs` - desktop web-terminal launcher: persists the global preference,
  prevents launch recursion with `CLUD_WEBTERM`, and forwards the original
  clud argv to the separately packaged Tauri companion. See
  [`docs/architecture/web-terminal.md`](../../../docs/architecture/web-terminal.md).
- `kitty_term.rs` - explicit `clud --kitty-term` launcher for an adjacent native
  WezTerm GUI and `clud-kittyterm.lua` config on Windows; forwards the original
  backend argv and fails if the pinned companion assets are unavailable.
- `bridge_log.rs` - issue #772's always-on, bounded JSONL writer
  for `~/.clud/state/sessions/<pid>__<epoch>/bridge.jsonl`; test-mode logs use
  the isolated sibling tree `~/.clud/state/test-sessions/`. Buffers complete
  lines across concurrent bridge workers, emits one visible truncation marker,
  and records failures plus (since #999) the model-discovery handshake.
- `cache_health.rs` - issue #1226's bounded, content-free per-conversation
  terminal-usage aggregates and cache-credit safety-fuse state machine. It
  accepts only provider counts keyed by the bridge's digest-derived scope;
  compaction, clear, and provider-route boundaries begin a new cold window.
- `anthropic_usage.rs` - terminal usage adapters for native Claude, DeepSeek,
  and OpenRouter Messages responses. It observes copied SSE frames or a
  bounded non-streaming JSON body, retains only terminal counters, and leaves
  forwarded bytes unchanged.
- `codex_bridge.rs` - issue #626's authenticated, loopback-only HTTP shell and
  #898/#899's unified Claude/Codex/DeepSeek multiplexer: ephemeral listener +
  per-launch bearer, deterministic `/v1/models`, provider catalog routing,
  strict credential isolation, request-time effort mapping, route epochs,
  preservation of the launch wire-model pin when Claude sends its discovery ID,
  native Claude token counting, bounded parser/workers/timeouts, authenticated
  context compact/finalize/clear lifecycle controls, and joined shutdown.
  Per-phase header/body deadlines and a per-frame idle timeout preserve chunked
  progressive SSE; see `docs/architecture/unified-gateway.md`. The upstream hop
  of the Anthropic passthrough proxy reads byte-idle rather than on a
  whole-request deadline, reports a mid-stream failure in-band instead of
  closing like a clean end, and answers a timeout 504 rather than 502 (#1263,
  DD-028 amendment, DD-079). Before committing a successful Messages response,
  it classifies non-Message JSON and zero-event SSE as provider faults (#1698);
  see `docs/architecture/provider-failover.md`.
- `serve_gateway.rs` - foreground API-server startup and credential setup; see [standalone-gateway.md](../../../docs/architecture/standalone-gateway.md).
- `serve_transport.rs` - tracked standalone upstream TLS sockets and shutdown cancellation.
- `codex_model.rs` - #752's Codex compatibility view over the shared provider
  catalog: the `sol`/`terra`/`luna` aliases and per-model defaults, the
  `<model>@<effort>` parser, and the provider-neutral `Effort` ladder
  restricted to what the family accepts (no `minimal`, no `ultra` --
  re-confirmed against OpenAI's model guidance in #821). Selection rides the
  model string because that is the one field a custom `ANTHROPIC_BASE_URL`
  gateway is allowed to own, so it cannot be dropped in transit the way
  `output_config` can (DD-035). The compound spelling remains a compatibility
  input; current Claude launches advertise provider-scoped discovery IDs and
  carry ordinary effort separately.
- `codex_runtime.rs` - #1476/#1480's bounded Pages model-manifest fetch and
  paginated local Codex `model/list` availability gate. It maps stable
  Sol/Luna catalog and discovery IDs to current wire IDs with independent
  per-family last-good then built-in fallback;
  see [model-pages.md](../../../docs/architecture/model-pages.md).
- `codex_translate.rs` - pure Anthropic Messages -> OpenAI Responses request
  mapping: typed in/out structs, transcript-order-preserving tool loops,
  auth-mode-dependent system placement, reasoning round-trip, and bounded
  reversible identifiers. Translation is **total** -- droppable Anthropic
  fields are dropped, not rejected (#750, DD-030); a `tool_use` with a
  missing/`null` or stringified `input` still replays as object `arguments`
  (#1533). The carve-out is a stated
  effort the family does not accept (`output_config.effort`, `@effort`): that
  is a 400 naming the accepted values, because the only alternative is running
  the turn at an effort the user never chose (#821, DD-035).
- `codex_sse.rs` - #627 step 3: `FrameDecoder` (byte-level SSE framing,
  fragmentation/CRLF/heartbeat tolerant) and `StreamTranslator` (Responses
  events -> Anthropic events with monotonic block indices). A tool block never
  opens before its id and name are known. HTTP-free; wired in at step 5.
- `codex_upstream.rs` - #627 step 4: `CredentialSource` (so #629's
  subscription auth reuses translation unchanged) plus a streaming
  `POST /v1/responses` client with connect/read/overall timeouts, bounded
  buffering, cancellation, and a retry policy that stops the moment any byte
  reaches the sink. Builds every outbound header itself, so no downstream
  header can leak upstream. #764 added `UpstreamFailure`/`FailureClass`: the
  error response is read (bounded body prefix, `cf-ray`, `x-request-id`,
  `Retry-After`), classified permanent/transient/unknown, and dropped — the
  class picks the retry budget, backoff is exponential with jitter, and a
  narrow observer reports each attempt/backoff to the bridge forensic log. See
  [`../../../docs/architecture/launch-targets.md`](../../../docs/architecture/launch-targets.md)
  and DD-032.
- `auth.rs` - #898's action-first `clud auth login|status|logout <provider>`
  dispatcher and secret-free aggregate provider status. Hidden compatibility
  aliases retain provider-owned implementations and print an exact replacement.
- `codex_auth.rs` - #629's clud-owned Codex credential implementation: browser
  authorization-code + PKCE callback flow, separate `~/.clud/codex-auth.json`
  store, safe status claims, locked atomic refresh with bounded transient
  retry and forced recovery after an upstream 401, and token-redacted
  diagnostics. See
  [`../../../docs/architecture/launch-targets.md`](../../../docs/architecture/launch-targets.md).
- `provider_auth.rs` - shared API-key credential implementation (originally
  #877's DeepSeek-only `deepseek_auth.rs`, generalized by #937): hidden terminal
  input, OS-native credential vault adapter parameterized on the vault
  service/account identifiers, injectable in-memory test store, and secret-free
  status/error surfaces.
- `credential_chain.rs` - the vault-first credential chain behind
  `NativeSecretStore` (#1891): native vault, then an owner-only
  `~/.clud/credentials` file, then the provider's API-key env var (read only).
  See [DD-174](../../../docs/DESIGN_DECISIONS.md#dd-174-api-key-credentials-fall-back-to-an-owner-only-file-and-the-env-var).
- `provider_registry.rs` - the `AnthropicCompatProvider` descriptor table
  (#937/#939): per-provider vault identifiers, base URL, CLI flag, optional
  Claude role-model profile, and child-env behavior for providers that speak
  the Anthropic API directly. DeepSeek, Kimi, and OpenRouter share this path;
  adding a provider is primarily a row here plus a `ModelProvider` variant,
  not a new auth or transport stack.
- `codex_pipeline.rs` - #627 step 5: chains translate -> upstream -> SSE into
  one call, plus `MessageAggregator` so a non-streaming request reuses the
  streaming state machine. Owns the downstream status policy — since #764,
  `502` means only a genuine gateway failure, and `TooLarge`/`Cancelled`/
  `Downstream` map to `413`/`499`/`499` instead of borrowing it.
- `failover.rs` - #968: the ordered, cost-labeled failover ladder. A rung is
  either a catalog model (resolved to its provider wire ID) or an ordinary
  Claude model ID forwarded verbatim, because clud owns the synthetic
  provider namespaces while Anthropic owns its own inventory. Descent
  consults `route_health::RouteLedger` and withholds `CostOwner::Metered`
  rungs until consent is recorded. Design:
  `docs/architecture/provider-failover.md`.
- `route_health.rs` - #968: reads an `UpstreamFailure` as a statement about
  its *route* rather than its attempt (`RouteVerdict`: healthy /
  throttled / exhausted / drained / unauthenticated / request-fatal) and
  keeps a launch-scoped `RouteLedger` of which routes can serve and until
  when. Clocks are passed in, never read, so every rule is testable
  without sleeping. Design: `docs/architecture/provider-failover.md`.
- `codex_history.rs` - bounded, in-memory canonical Responses transcript for
  the foreground bridge. It commits only newly pending input plus opaque
  upstream output, evicts at bridge shutdown, performs the two-phase harness
  fallback after failed compaction, and makes validated provider compaction
  replacement and unified provider-route epoch changes atomic; see
  [codex-via-claude.md](../../../docs/architecture/codex-via-claude.md) and
  [unified-gateway.md](../../../docs/architecture/unified-gateway.md).
- `route_plan.rs` - the one place a launch's routing is decided: `ResolvedRoute`
  (backend, upstreams, slots, allowlist, discovery, context, effort, upstream
  routing, timeout, scrub list, launch checks), the pure resolver, and every
  backend's renderer. Launch checks run from it before the backend branch. See
  [route-plan.md](../../../docs/architecture/route-plan.md) (#1852).
- `openrouter_routing.rs` - provider pin/privacy request-body renderer and
  the early `--set-openrouter-zdr` setter. Saved policy resolution and audit
  provenance are described in [provider-selection.md](../../../docs/architecture/provider-selection.md).
- `foreground_runtime.rs` - shared foreground lifetime owner and injectable
  subprocess/PTY environment-spawn seam. It conditionally owns the direct Codex
  bridge or unified gateway, preflights direct Codex bridge credentials before
  creating a listener or child (including daemon admission), and on an
  interactive missing-credential or rejected-refresh bridge launch can
  explicitly offer to copy a usable Codex CLI login into clud's own credential
  store, with identity-aware repair/switch prompts (daemon and
  non-interactive admission never prompt). It applies child-local
  overlays, emits sanitized optional-provider notices and the image-capability
  notice for a model whose endpoint drops pasted images upstream (#1200),
  registers launch-scoped authenticated `PreCompact` and
  `SessionStart(clear)` HTTP hooks, and tears the listener down on every
  runner return path. Unified mode enables discovery while preserving Claude
  credentials and ambient session effort; the direct Codex route enables the
  same protocol with a Codex-only catalog and child-local 1.05M context
  metadata. Direct Anthropic-compat providers (DeepSeek, Kimi, OpenRouter)
  carry the catalog's `low` effort default on the harness's `--effort` session
  flag and never pin or scrub `CLAUDE_CODE_EFFORT_LEVEL` (DD-059). The direct
  overlay also pushes `API_TIMEOUT_MS` as a default and the launch names the
  effective value, because this is the one route with no clud process in the
  request path and therefore no byte-idle watchdog (#1263, DD-079).
- `shell_ast/` - shell commands parsed into syntax trees (tree-sitter bash and
  PowerShell, a cmd.exe splitter) and evaluated without running anything, for
  the command scanner (#1962); see [`shell_ast/README.md`](shell_ast/README.md)
  and [shell-ast.md](../../../docs/architecture/shell-ast.md).
- `shell/` - shell-policy plumbing: lazy fetch of a vendored portable Git
  Bash bundle (`shell/git_bash_resolver.rs`) so callers can hand
  `CLAUDE_CODE_GIT_BASH_PATH` to Claude Code without depending on a
  system-wide Git for Windows install. Manifest at
  `vendor/win32/git-bash-bin.toml`; cache at
  `~/.clud/vendor/win32/git-bash-bin-<sha[..12]>/`.
- `startup.rs` - launch-time helpers factored out of `main.rs`: drag-target
  gating (`--no-dnd`, `--dry-run`), session-cap enforcement, Ctrl+C flag
  installer.

CLI surface and backend resolution:

- `args.rs` - `clap` `Args` and `Command` definitions; passthrough for unknown
  flags; subcommand definitions for `loop`, `up`, `rebase`, `fix`, `do`, `gc`, etc.
- `backend.rs` - concrete `Backend`, independent `ModelProvider` /
  `HarnessSelection` resolution, provider `RoutingMode`, process `LaunchMode`,
  PATH lookup, and backend-path resolution. See
  `docs/architecture/launch-targets.md` and
  `docs/architecture/provider-selection.md`.
- `harness_picker.rs` - installed Claude/Codex/DeepSeek discovery plus the
  three-second bare-launch selector and its pure choice/countdown model.
- `home.rs` - the only resolver of the user's home directory
  (`user_home`; Windows `USERPROFILE` > `HOME` > OS, else `HOME` > OS).
  Library `home_dir` calls and raw `USERPROFILE` reads are banned elsewhere
  ([DD-161](../../../docs/DESIGN_DECISIONS.md#dd-161-the-users-home-directory-has-exactly-one-resolver)).
- `dsh_harness.rs` - DeepSeek Harness managed install (`clud dsh-update`),
  private Node runtime, child key passthrough, and the OpenRouter `--patch`
  overlay (#1829). See
  [`launch-targets.md`](../../../docs/architecture/launch-targets.md#deepseek-harness-install-and-providers).
- `video.rs` - `clud video`: browser-use/video-use as a session-scoped Claude
  plugin, never a global skill (#1851). See
  [`launch-targets.md`](../../../docs/architecture/launch-targets.md#clud-video-video-use-as-a-session-scoped-plugin-1851).
- `provider_catalog.rs` - the single registry mapping stable clud model IDs,
  gateway discovery IDs, provider wire IDs, compatibility aliases, and
  independent effort/context capability metadata.
- `selector.rs` - the one inline terminal selector (#1195, DD-073). It owns
  raw mode, cursor hide/show, pending-input draining, key decoding, CRLF-only
  rendering with wrap-aware row counts, and redraw/erase. The launch-scope
  prompt, the harness picker, and `clud settings` each implement its
  `Selector` trait and render through it.
- `preference.rs` - shared pure typed-choice state machine used by launch
  scope and global settings selectors.
- `subprocess.rs` - single decision point for the Windows `.cmd`/`.bat`
  rewrite (BatBadBat / CVE-2024-24576) via `running-process-core`'s
  `CommandSpec::Shell`.

Console and terminal:

- `console_input.rs` - (now `platform/windows/console_input.rs`, re-exported from `lib.rs`) Windows adapter over
  `running_process::pty::terminal_input::TerminalInputCore` (issues #141 /
  #575): runs its own `ReadConsoleInputW` loop so surrogate pairs are joined
  first (#1351), forwards upstream's complete virtual-key translations atomically
  while retaining clud's Shift+Enter-as-ESC-CR (ConPTY rewrites LF, #1369) and Ctrl+V image policies.
- `console_surrogates.rs` - platform-neutral `SurrogatePairer`: joins the two
  UTF-16 surrogate key records of an emoji into UTF-8, across read batches,
  with U+FFFD for an unpaired half (#1351). See
  [windows-quirks.md](../../../docs/architecture/windows-quirks.md#d-native-terminal-input-via-running-process-issues-141--575--1351).
- `console_setup.rs` - `enable_console_vt_output`, which `main` calls first
  to enable `ENABLE_VIRTUAL_TERMINAL_PROCESSING` on the stdout and stderr
  consoles for the process's life (#1374, DD-105), and the RAII guard that
  enables `ENABLE_VIRTUAL_TERMINAL_INPUT` on stdin and VT processing on a
  terminal stdout (#1345) for the lifetime of a PTY session and restores the
  prior console modes on drop; no-op on POSIX.
- `console_title.rs` - stamps `clud <cwd-name>` once on launch and runs a
  background keeper that re-applies the title when downstream OSC 0/2 sequences
  overwrite it.
- `console_title_osc.rs` - stream-resumable OSC 0/2 filter used by the PTY
  output path; re-exported through `console_title` to preserve its call sites.
- `codex_lf.rs` - Codex-only stream-resumable bare-LF→CRLF filter chained
  after the OSC stripper on the PTY output reader (#1181, DD-070); masks
  Codex's goal-cell rendering bug on Linux/macOS the way the Windows console
  does natively. Never enabled for other backends.
- `capture.rs` - server-side terminal emulator (`vt100` + `vte` sticky-mode
  sniffer) that lets the daemon synthesize a repaint when a mid-session client
  attaches.
- `session.rs` - raw-PTY pump (`run_raw_pty_pump`), resize handling, F3 voice
  observer hook, OSC-title stripper integration, dropped-path injection on the
  PTY master. Since issue #538 the pump splits output onto a dedicated
  reader thread + stdout-writer thread (`run_output_writer`) so a slow
  terminal flush never delays stdin forwarding — see
  `docs/architecture/session-lifecycle.md` and DD-018. Since #1189 the writer
  thread optionally runs the toast compositor
  (`session_output.rs::run_output_writer_composited`) and the stdin path runs
  the toast close-button mouse filter; see `docs/architecture/toasts.md`.
  The reader also balances child-owned kitty keyboard-enhancement frames on
  forced shutdown (#1221), without resetting terminal state that predates
  the session. `RawTerminalGuard`'s exit reset turns off every input mode a
  child enables (mouse, focus `?1004`, bracketed paste `?2004`) and shows the
  cursor (#1701).
- `session/escape_gate.rs` - keeps an escape sequence from reaching the PTY
  split across two writes (#1697).
- `session/child_modes.rs` - the terminal modes a child TUI left on (alternate
  screen, scroll region, cursor-key and keypad modes, colours, mouse, focus,
  paste, cursor), so the session can undo exactly those after Ctrl+C kills it
  (#1704, DD-147). See
  [session-lifecycle.md](../../../docs/architecture/session-lifecycle.md#terminal-restore).
- `terminal_queries.rs` - `TerminalQueryScanner`, the stub replies to a
  child's terminal queries when no real terminal answers, shared by the local
  pump and the daemon worker and resumable across PTY reads (#1310, #1347,
  #1702).

Loop subsystem (`clud loop`):

- `loop_spec.rs` - task-spec resolver: classifies the positional (GH URL,
  `#42`, file, literal), fetches GH issue/PR bodies via `gh` (curl fallback),
  caches under `.clud/loop/`, locates DONE/BLOCKED marker files.
- `loop_check.rs` - post-iteration DONE/BLOCKED marker check; file-only and
  stdout-scanning variants used by PTY and subprocess paths respectively.
- `loop_artifacts.rs` - durable `<git-root>/.clud/loop/` artifacts:
  `info.json` (`TaskInfo`), `log.txt`, `motivation.md`, and `.gitignore`
  auto-injection.
- `stream_json.rs` - pure renderer for claude's `--output-format stream-json`
  events; turns one JSON event per line into one human-readable progress line
  for subprocess-mode loops.

Process management and GC. The cross-directory story — the two disjoint
reapers, the `(pid, creation_time)` keyspace, daemon-sparing precedence, and
which test tier a change belongs in — lives in
[`docs/architecture/process-reaping.md`](../../../docs/architecture/process-reaping.md).

- `process_identity.rs` - `ProcessIdentity` = PID **plus** OS start time, and the
  comparison every path that stores a PID and acts on it later must go through
  (issue #558). A bare PID is not a stable handle: Windows reissues numbers
  promptly, so "the PID is alive" and "that process is alive" are different
  questions. Records written before the field existed carry
  `UNKNOWN_START_TIME` and fall back to a PID-only comparison. Terminated
  states are not live; the native zombie regression lives under
  [`platform/`](platform/README.md).
- `process_scan.rs` - clud's own host **environment** pass on `sysinfo 0.37`
  (#673 Phase 7): `scan_env()` answers both env questions from one snapshot,
  and `DaemonMarkerCache` reads each identity's environment once, ever, over a
  bounded candidate set.
- `process_tree.rs` - best-effort descendant-tree termination via `sysinfo`;
  fixes the multi-second Ctrl+C hang for `clud --codex` on Windows where
  `cmd.exe -> node.exe` would orphan the real child. `TopologySnapshot` is one
  host walk reused across a whole sweep, and re-verifies `(pid, start_time)`
  for **every** target it kills, descendants included (#688).
- `job_orphan_reaper.rs` - foreground Job reap decision model; the Windows completion-port tracker is in `platform/windows/job_orphan_reaper.rs`.
  The runner registers each backend root by PID + start time; a pure role
  planner recognizes the exact Claude/Codex host, direct tool shells, Git Bash
  handoffs, nested detachments, declared daemons, and the unconditional
  `conhost.exe` and sidecar `OpenConsole.exe` exclusions before any automatic
  client-tree reap (#616, #1367).
  Daemon-sparing goes through the `ProcessFacts` seam and its precedence
  ordering (#673 Phase 1a); the tracked keyspace is bounded by one purge sweep
  (Phase 2). The seam itself now lives in `reaper_facts.rs` — this module adds
  only the Job Object signal and its own tool-shell reap reasons.
- `reaper_facts.rs` - the OS-authoritative daemon-sparing signals, shared by
  **both** reapers (#688). `ProcessFacts` / `FactsSnapshot` / `spare_signal`
  are pure data plus one fenced per-platform producer, which is what keeps the
  precedence table unit-testable everywhere. `collect_host_facts` is the
  producer `orphan_reaper` uses; it reports job membership as *unavailable*,
  because there is no Job Object on the `clud slay` / on-exit / daemon-sweep
  path.
- `reap_log.rs` - reaper accounting (`ReapCounters`), buffered mutations-only
  JSONL at `~/.clud/state/sessions/<pid>__<epoch>/reap.jsonl`, and a durable
  five-second `reap-health.json` flight-recorder checkpoint for watchdog-reset
  forensics. It records host scans and adaptive-backoff deferrals without
  synchronous per-event logging.
- `session_registry.rs` - `redb`-backed registry of live `clud` PIDs that caps
  concurrent siblings; `Drop` removes the row, startup GCs dead rows. Exit-time
  removal is bounded by `SHUTDOWN_BUDGET` (#1660) so a held lock cannot stall exit.
- `gc/target_size_cache.rs` — daemon-measured per-worktree Rust target sizes;
  `gc list --targets` reads the cached report ([contract](../../../docs/architecture/gc-and-registry.md#cached-rust-target-sizes-1687)).
- `gc/worktree_registrations.rs` — dead `git worktree` registration count + prune (#2397);
  `clud gc prune worktree-registrations` ([contract](../../../docs/architecture/gc-and-registry.md)).
- `gc/session_tmp_cap.rs` — `tmp.max_bytes` size cap for `~/.clud/tmp` (#2397, DD-194); `clud gc prune tmp`.
- `gc/undeletable.rs` — aggregates deletion failures into the single "N paths could not be removed" line (#2397).
- `gc/` - `clud gc list` / `prune` / `purge` / `all` / `reconcile` CLI handlers and
  daemon-watch root derivation. The GC registry and its shared watcher live inside
  the daemon. `delete_audit.rs` is the pre-deletion JSONL audit trail
  (`<state>/gc-audit.jsonl`, issue #893) written by every deletion path that can
  touch user data, plus the target-sweep over-match guard. `worktree_root.rs`
  owns `~/.clud/worktrees` (issue #1485), the agent worktree root outside the
  `session_tmp` sweep, its ordinal allocator `alloc_wt_path` (#1486), plus
  the warn-only `worktrees.warn_bytes` size check; `worktree_size_cache.rs`
  caches that check from the daemon probe for the launch banner (#1610),
  and the same warn-only check for `~/.clud/tmp` behind `tmp.warn_bytes` (#1327),
  and for `~/.clud/cache` behind `cache.warn_bytes` (#1691);
  see [gc-and-registry.md → worktree root](../../../docs/architecture/gc-and-registry.md#the-worktree-root-cludworktrees-1485).
  `uv_cache_repair.rs` invalidates live uv wheel pointers to gutted archives
  under uv's own locks (#1711, DD-148).
- `worktrees.rs` - `--clean-worktrees` (issue #83): enumerates via
  `git worktree list --porcelain`, classifies clean / dirty / unpushed / gone,
  removes safe ones; `--dry-run` faithful. `worktrees_verdict.rs` (#1606)
  first consults the daemon's squash-aware verdict through
  `daemon::repo_worktree_cli`, so landed work is removed by the daemon's
  executor; see [gc-and-registry.md → `--clean-worktrees`](../../../docs/architecture/gc-and-registry.md#--clean-worktrees).
- `optimize.rs` - `clud optimize rust`: installs/persists soldr defaults and
  writes repo-local `.clud/settings.json` directives.
- `repo_clud_config.rs` - `.clud/settings.json` discovery + parser, both
  repo-level and user-level (two-level merge, repo wins per scalar field).
  Owns the `rust.use_soldr` activation schema (DD-014) and the generic
  `bad_commands` rule schema (DD-016) — repo maintainers add their own
  "bad command → blessed replacement" rules here (e.g. banning bare
  `playwright` in favor of a project's `npm run test:integration`); see
  DD-016 for the full field reference and a copy-pasteable example.
  `bad_commands` concatenates across repo/user levels instead of
  overriding, unlike the scalar `rust.*` fields. Each parsed rule carries a
  non-serialized `RuleSource` (file, layer, `/bad_commands/<index>` slot) so a
  denial can cite exactly which rule fired and where it came from (#525);
  provenance survives merge/dedupe, so a shadowing rule keeps its own origin.
- `block_bad_cmd.rs` - native `cmd-scan` PreToolUse hook binary (formerly
  `block-bad-cmd`; `clud-block-bad-cmd` is still an argv[0] alias of `clud`, see
  `multicall.rs` and `block_bad_cmd_rollout.rs`). Enforces three things per Bash command:
  hardcoded Rust-toolchain rules (`RUST_TOOLS` → `soldr <tool>`, only when
  the cwd is inside a `Cargo.toml`/`rust-toolchain` tree, #2065); GitHub PR
  waiter rules (run-id watches and polling loops; the canonical PR-check
  watch is allowed with the session alias, DD-116), gated behind the
  `git.pr_wait_fail_fast` toggle (on by default, see `settings_tui.rs`; the guard sees
  through the `tap` gate prefix); and the config-driven `bad_commands`/`bad_pipelines`
  engine from `repo_clud_config.rs` (DD-016/DD-017). Also eager-GC-tracks
  `git clone`/`git worktree add` destinations and guards clones outside
  `.extern-repos/` (zackees/clud#532). The scanner splits shell segments and
  unwraps nested shells/`eval`/command-substitution before matching;
  `CLUD_BAD_CMD_OVERRIDE` is the per-rule escape hatch. Config denials cite
  provenance (matched token, rule id, `<file>#/bad_commands/<index>`) in the
  reason and log a structured `bad_cmd_denied` event to
  `~/.clud/tools/hooks/block-bad-cmd.log` (#525). User-facing rule-writing
  guide lives in the root `README.md`. Also enforces `bash.block_cd` via
  `block_bad_cmd_cd.rs` (see below), after the command rules so an
  independently forbidden command reports the stronger reason. And it is the
  dispatcher for Tier B (#966 §6, #967 Phase 4): `declared_hook_denial`
  resolves the fire targets once per call (parent, then distinct child/extern
  roots), runs each root's hooks rooted at the root — the parent's hooks only
  for parent/child-owned paths, never extern-owned — layers any deny, and
  gates child/extern execution behind the codex project trust table
  (`hook_health/codex_trust.rs`) and extern roots behind `hook_trust.rs`
  (DD-060/DD-061). A `CwdChanged` event bypasses the scanner entirely: it is
  handed to `block_bad_cmd_cwd_changed.rs` (see below), the #967 Phase 5
  backstop that reacts to drift the PreToolUse scan cannot see. Also the
  opt-in recursive-delegation guard (#812, DD-069): with
  `CLUD_BLOCK_RECURSIVE_AGENTS` set, `recursive_agent_decision` denies an
  `Agent`/`Workflow` call whose payload carries an `agent_id` (the harness sets
  it only inside a subagent), so effective direct-delegation depth is 1. Runs
  ahead of the command rules because an `Agent` call has no command to scan.
- `clud_hooks.rs` - `.clud/hooks.json` schema, discovery, and matcher
  semantics (#967 Phase 2), plus the Tier B source for sub-repos (#967 Phase 4,
  D4): `from_frontend_settings` reads `.claude/settings.json`,
  `.claude/settings.local.json`, and `.codex/hooks.json` (in that order) and
  parses the `hooks.<Event>` group shape and the legacy direct shapes,
  including codex's root-level `<Event>` legacy shape. Non-`command` handler
  types are skipped, `hooks.state` (codex's trust table) is never an event,
  and entries dedupe across files by (event, matcher, command). Matchers are
  anchored regexes over the tool name; an uncompilable one falls back to
  equality so a declaration clud cannot parse under-matches rather than firing
  someone's guard against tools they never named. Parsing is lenient per
  entry, like `repo_clud_config`.
- `extern_root.rs` - where a repo's foreign checkouts live (#986). Beside the
  repo (`~/dev/myrepo` -> `~/dev/myrepo-extern/`), not inside it, so no tool
  pointed at the repo can reach them and no exclusion has to be maintained.
  Derives from the **main** repo root, so worktrees share one directory rather
  than cloning a dependency each. `sibling_for` is fallible (a repo at a
  filesystem root has no parent), and `known_roots` keeps returning the legacy
  in-tree `.extern-repos/` so existing checkouts stay discoverable and clonable
  during migration. Claiming (`claim_state`/`claim`) refuses a non-empty
  directory clud did not create, because the name is guessed from the repo's
  own and guessing wrong must not scatter clones through somebody's project.
- `extern_cli.rs` - `clud extern trust <name>` (#967 Phase 4, DD-060): the
  one-command enable for a foreign checkout's hooks. Records `{name, origin}`
  in the parent's `hook_trust` store (origin read from the checkout's
  `.git/config` in-process), plus `--list` and `--revoke`; the parent root is
  resolved via `block_bad_cmd::nearest_repo_root_public`, so the command works
  from anywhere under the parent.
- `clud_hook_roots.rs` - the typed hook-root registry (#966 §5-6, #967 Phase
  3). `parent` (session root), `extern` (immediate children of
  `.extern-repos/`, implicit), `child` (declared in `.clud/settings.json`;
  nested git repos are deliberately **not** auto-detected, because declaration
  is the consent the child tier's no-prompt trust rests on). The kind decides
  whether the *parent's* hooks fire: never in an `extern` root, always in a
  `child`. Containment resolves from what a call names (`tool_input_paths`),
  falling back to `cd` targets and only then to cwd — keying on cwd alone
  would answer "parent" for a subagent's edit inside a sub-repo. Roots clud
  resolves at launch that a hook cannot rediscover travel in `CLUD_HOOK_ROOTS`
  as JSON.
- `hook_trust.rs` - the Tier-B trust store (#967 Phase 4, DD-060/DD-062): the
  `hook_trust.extern` allowlist in the parent's gitignored
  `.clud/settings.local.json`, keyed by checkout name + origin URL. `load` is
  lenient (corrupt or missing store = empty allowlist), `record`/`revoke` are
  read-modify-write preserving every other key in the file, `is_trusted`
  matches origin exactly or by name alone for origin-less checkouts, and
  `origin_of` reads `.git/config` in-process (both remote section spellings,
  quoted values, worktree `gitdir:` files) without spawning git.
- `clud_hooks_compile.rs` - compiles declarations into a frontend's native
  registration (#977, #967 Phase 2b). One dispatcher line per declared event,
  matcher `*` — the per-hook matcher is applied by clud at dispatch time, so
  narrowing here would silently drop declarations an installed `Bash`-scoped
  line could never deliver. `merge_hook_settings` concatenates per event rather
  than replacing, mirroring how the harness layers hooks, so clud's
  registration cannot displace the bridge's lifecycle hooks or a user's own
  `--settings` document. Codex receives its PreToolUse command hook and trust
  state through launch `-c` values, without writing to user configuration.
  Phase 5 adds the `CwdChanged`
  backstop (`CWD_CHANGED_EVENT`, DD-064): `claude_settings_fragment` inserts
  the line only where the capability probe in `backend_bootstrap.rs` says the
  installed Claude fires the event, and a repo's own declared `CwdChanged`
  hook shares that same line instead of registering twice.
- `clud_hooks_run.rs` - Tier-B execution. Runs each declared hook with cwd and
  `CLUD_PROJECT_DIR` set to the declaring repo's root, the harness payload
  forwarded on stdin (pipe closed, so a hook blocking in `json.load(sys.stdin)`
  gets EOF), and the harness's exit-code contract: 0 continues, 2 blocks and
  relays the child's own words, anything else — including a timeout — warns and
  continues. Failing open there is deliberate: a broken guard must not become a
  wall in front of every tool call. The only in-repo user of
  `StdinMode::Piped`. See `docs/architecture/hook-dispatch.md`.
- `block_bad_cmd_cd.rs` - `bash.block_cd` session-cwd pinning (#967 Phase 1,
  DD-047; Phase 5 relaxation = DD-063). Scans for `cd`s that would move the
  *session* cwd — subshells, `$(...)`, and nested shells are skipped because
  they cannot leak — resolves the target against the registered roots (Phase
  3's `CLUD_HOOK_ROOTS`: parent, children, and extern trees) and denies per
  policy. `"auto"` is a three-level resolver at fire time: **strict** whenever
  any cwd-sensitive raw hook is in scope (a relative script path pins the
  cwd), **relaxed** when the repo is fully dispatcher-managed — a
  `.clud/hooks.json` opt-in (`dispatcher_managed`, recorded by the scan even
  with no raw hooks; an empty file is not an opt-in) with no sensitive raw
  hooks — denying only escapes of the whole registered-root set, and **off**
  with no hooks or outside any repo. Declared-hook command text never counts
  toward sensitivity because the dispatcher roots it. The classifier reads
  `.claude/settings*.json` and `.codex/hooks.json` across **all** events,
  deliberately not through `hook_health::inspect`, which parses only
  `PreToolUse` — the wedge that motivated the issue came from a `Stop` hook.
  `hook_health` reuses it for the broken-`git rev-parse`-prefix warning.
  `drift_warning` is the pure predicate the `CwdChanged` backstop (next
  entry) uses. Every settings/hook read sits behind a word-boundary
  pre-filter so a command with no `cd` pays one lowercase scan.
- `block_bad_cmd_cwd_changed.rs` - the #967 Phase 5 `CwdChanged` backstop
  (DD-064): reactive detection of session-cwd drift the PreToolUse scanner
  cannot see (aliases, scripts that chdir). Dispatched by `block_bad_cmd.rs`
  on the explicit event, it resolves `bash.block_cd` against the session
  parent root (`CLAUDE_PROJECT_DIR` — it stays put while cwd drifts — with a
  walk-up fallback to the repo the session landed in), prints a `[clud]
  CwdChanged:` hygiene warning when the payload's `new_cwd` violates the
  policy, and still runs a repo's declared `CwdChanged` hooks rooted at the
  repo (Tier B). It **never blocks**: exit 2 from a declared hook is
  downgraded to a warning, because the cwd has already changed and the
  harness gives the event no decision control. Always exits 0, so no payload
  shape or harness misbehavior can turn it into a wall. The backstop line is
  registered only where a bounded per-launch capability probe
  (`probe_claude_cwd_changed_support` in `backend_bootstrap.rs`: `claude
  --version`, floor 2.1.83) says the installed client fires the event; a
  probe failure or a non-Claude harness degrades silently to no line.
- `block_bad_cmd_rm_vars.rs` - #963's POSIX/Bash abstract interpreter for
  catastrophic `rm`/`rmdir` operands rooted at `$VAR` or `${VAR}`. It rewrites
  only single, statically proven literal assignments into a complete
  `allow + updatedInput` hook response; unresolved, dynamic, conflicting, or
  root-like values become deterministic denials so unattended agents can retry
  without an approval prompt. It also owns `raw_payload_mentions_removal`, the
  word-boundary probe over *raw, unparsed* payload bytes that #1064's
  fail-closed path consults when the hook could not parse its input at all —
  separate from the command-text probe next to it, because raw bytes may be
  truncated mid-token, write newlines as `\n`, and name the program by path.
- `block_bad_cmd_grind_caps.rs` - per-role shell caps for the `/grind` agents, keyed on the payload's `agent_type` and the session's run facts (`grind_facts.rs`), plus the feature-mode router caps (`router_reason`) for callers without a grind role while those facts record a feature. The router-callable `grind-orchestrator` exception has an AST-backed read-only allowlist that rejects output redirections and command-local environment overrides; execution roles remain internal. See [grind.md](../../../docs/architecture/grind.md#role-caps).
- `block_bad_cmd_grind_gate.rs` - session-relative native task metadata lookup and exact approved-check authorization for the internal `grind-gate` role. `block_bad_cmd_grind_gate_evidence.rs` records one attempt per native gate agent and checks readiness reports against the matching native tool outcome.
- `block_bad_cmd_grind_resume.rs` - checks the current session transcript's native runtime version before graph resumption, refusing older or unknown runtimes without clearing deferred state.
- `block_bad_cmd_grind_resume_origin.rs` - records native Workflow launch provenance and binds resume admission to a supported original cache, session and script path.
- `block_bad_cmd_gate.rs` - the command gate: an **allowlist** requiring every
  statement in a shell tool call to be invoked through a wrapper (`tap` by
  default), so the wrapper sees post-expansion argv instead of the hook having
  to prove safety from command text. Off unless `CLUD_CMD_GATE` is set to an
  enabling value (DD-056 lists them) in
  the session environment. Unlike the rest of `block_bad_cmd`, it **fails
  closed** — an unreadable or unparseable payload is a denial, since an
  unverifiable command cannot be allowed. Reads each statement's literal words
  from the syntax tree rather than the scanner's rule words, whose
  wrapper-unwrapping would let `env tap ...` satisfy the prefix check. Refuses anything it cannot decompose with certainty (command
  substitution, subshells, process substitution, control flow); the cost of
  that is one extra tool call, not a wedged session. See DD-056.
  `classify` (re-exported as `block_bad_cmd::classify_for_gate`) grades a
  command by how much restating it needs; `gate_replay.rs` and the
  `clud-bench-gate-replay` bench binary use it to measure the gate's cost
  over recorded transcripts (#1067 step 2).
- `gate_replay.rs` - content-safe aggregation for that replay: extracts Bash
  commands from Claude Code transcript lines and counts gate classes, never
  command text.
- `settings_tui.rs` - `clud settings`: small cross-platform TUI checkbox menu
  over global boolean settings in `~/.clud/settings.json` (`clud_settings.rs`
  owns persistence). Pure `Menu` state machine + crossterm raw-mode I/O shell,
  same split as `launch_setup.rs`'s `ScopeSelector`. `--list` prints current
  values non-interactively. It does not expose `safe_rm.extra_roots`
  (#1668): the user adds that by editing the file by hand.

Platform glue:

- `trampoline.rs` - Windows-only rename-self-and-copy-back trick so
  `pip install` can always overwrite `Scripts/clud.exe`. No-op on POSIX.
  Also hosts `relay_child_and_wait`, the runtime-cache relay that must stay
  raw `std::process` (#333). The daemon detach moved to
  `daemon/client.rs::spawn_detached_daemon` (#1186).
- `win_creation_flags.rs` - `invisible_helper_creationflags()` returns
  `CREATE_NO_WINDOW` on Windows for daemon-helper spawns; `0` elsewhere so call
  sites stay portable.
- `large_file_guard.rs` - startup-time guard that warns about source files
  large enough to choke agents (issue #132). On a git repo the primary path is
  now the in-process **index pass** (`large_file_guard/index_pass.rs`, issue
  #556): `gix-index` mmap-parses `.git/index` (resolving the `.git`-file
  `gitdir:` indirection so a linked worktree reports against its own index) and
  reports tracked source files straight from the index's cached stat sizes —
  no tree walk, ~ms instead of ~240-400 ms. Entries whose cached size is
  untrustworthy (racily-clean, recorded as 0) get one targeted `stat` each. The
  two failure modes route differently, and the distinction is load-bearing:
  **no index** (not a git repo, broken `gitdir:` indirection) falls to the
  original `ignore`-crate parallel walker under its hard 1 s deadline, while a
  **corrupt/unparseable index** first takes one killable
  `git ls-files --debug` (`large_file_guard/ls_files_pass.rs`), which still
  reads cached sizes and still never touches the object database. Collapsing
  the two would give up the whole win on precisely the repos most likely to be
  large. Untracked-file coverage on the launch path is deferred to the
  daemon-side pass 2 (#551). Why the index — and why never
  `--format='%(objectsize)'` / `ls-tree`, which are 3-12x slower and can
  trigger a network fetch on a partial clone:
  [DD-022](../../../docs/DESIGN_DECISIONS.md#dd-022-the-large-file-guard-reads-the-git-index-in-process-not-the-worktree).
- `path_norm.rs` - fbuild/zccache-style `NormalizedPath` and separator-safe
  path-string helpers for cross-platform path keys, serialization, and
  executable names received from another OS. `canonicalize_plain` strips the
  Windows `\\?\` prefix (git cannot parse it) before a path goes to git, and
  `same_path` compares paths even after one side was deleted (#1628).
- `launch_context.rs` - per-session launch-context record (#1675): the
  context-window values the child receives, written at launch and bound to a
  hashed session id by `clud session-hook`; see
  [launch-plan.md](../../../docs/architecture/launch-plan.md#launch-context-record-1675).
- `launch_setup.rs` - session-only/global setup selector plus
  selected-backend persistent setup actions for skills and Codex hook
  normalization.

Skills and hooks:

- `skills.rs` - the only skill installer, over the only skill source tree
  (`assets/skills/`). Bundles slash-command skills via `include_str!` and
  installs them during global launch setup for the selected backend
  (`.claude/skills/`, Codex `.codex/skills/` gated on `.codex`) only when the
  backend home already exists. Writes a missing file; never touches a file the
  user has taken ownership of (`managed-by: clud` marker stripped); refreshes a
  clud-managed copy that diverges from the bundle modulo whitespace; writes
  nothing when the installed copy is current. Purges stale clud-managed copies
  from `.agents/skills/` and retired names in `PURGED_BUNDLED_SKILLS` from every
  backend's skills dir. See
  [DD-039](../../../docs/DESIGN_DECISIONS.md#dd-039-single-skill-installer-over-a-single-source-tree).
- `tools.rs` - `BUNDLED_TOOLS` registry of `assets/tools/` scripts, including
  `github/pr_merge.py`, `github/pr_merge_watch.py`, and `github/is_meta_issue.py` (prints JSON
  `{meta, sub_issues, task_list_refs}`; errors on `gh` failure, never reports
  "not meta"; #1404), and `diagnostics/transcript_report.py` (#1276,
  [DD-138](../../../docs/DESIGN_DECISIONS.md#dd-138-the-first-1276-slice-is-a-read-only-transcript-analyzer-not-a-repeated-call-guard)).
- `hook_health/` - `PreToolUse` hook parity diagnostics and `--fix-hooks`
  remediation. `warnings.rs` also groups project-syncing hook commands into
  one launch warning per settings file (#1739). `codex_trust.rs` owns
  `codex_project_trusted` — the
  `[projects."<key>"] trust_level = "trusted"` check against
  `~/.codex/config.toml` that gates child/extern Tier-B hook execution in
  codex sessions (#967 Phase 4), and the `add_codex_project_trust` repair
  behind `clud --fix-hooks`.
- `block_bad_cmd_rollout.rs` - startup health/migration for the native
  `clud-cmd-scan` helper (an alias of `clud`): exact old hook command
  rewrites, including the legacy `clud-block-bad-cmd` name. Every scope only ever gets
  the portable bare `clud-cmd-scan`, never an absolute path (#1426, #1334);
  when the helper is not already resolvable, the session links `clud` alone (as `clud-cmd-scan` and
  `clud-block-bad-cmd`) into `~/.clud/state/helper-bin` and appends that directory to PATH (never the
  install directory, whose other programs would leak into descendants), and
  launch warns about hooks whose program does not exist.
  `ci/banned_hook_paths.py` keeps committed hook configs portable.
- `codex_hook_normalize.rs` - issue #234: idempotent Codex global-setup pass
  that bumps any `~/.codex/hooks.json` handler `timeout: 5` to `30`
  (`~/.clud/settings.lock` fs4 guard, green status line on change).
- `codex_cmd_scan_install.rs` - issue #1972: Codex global-setup pass that
  keeps a `matcher: "*"` `clud-cmd-scan` PreToolUse entry in
  `~/.codex/hooks.json` and its `hooks.state` trust in `~/.codex/config.toml`
  (same lock; never duplicates, never clobbers malformed files). See
  [hook-dispatch.md](../../../docs/architecture/hook-dispatch.md#global-codex-hook-1972).

Diagnostics and misc:

- `cpu_banner.rs` - issue #466: foreground CPU-burn banner. Spawns one
  background `sysinfo` sampler that ticks every 2 s, sums `cpu_usage()` +
  `memory()` over the parent-PID subtree rooted at our originator, and
  publishes a `cpu N % · X.Y / Z cores · …` toast (#1189: never stderr, which
  corrupted the harness TUI; see [toast/](toast/README.md)) when subtree CPU crosses
  `max(50 %, 0.20 × num_cpus × 100 %)` for 3 sustained ticks. Hysteretic
  drop-out at 0.7×; 30 s heartbeat while sustained; clear-banner only after
  ≥ 60 s episodes. Wired into `runner::run_plan_subprocess` and
  `runner::run_plan_pty` via a `BannerWatcher` whose `Drop` joins the
  thread. Suppressed by `--no-cpu-banner`, `--dry-run`, `--detach`,
  `--detachable`, `--repeat`, and `[foreground.cpu_banner] enabled = false`
  in `~/.clud/settings.json`. Slice of #463 (`clud top`).
  Issue #709: the full-system subtree rebuild (the dominant cost — #553
  measured 225 ms loaded, 2.09 s saturated) backs off 30 s → 120 s via
  `RebuildCadence` while the subtree stays under `REBUILD_QUIET_PCT`. Any
  activity resets the cadence *and* forces an immediate rebuild on the next
  tick, so a busy session is never sampled against a stale pid list — the
  backoff is only ever paid for by an idle one.
- `wedge_watchdog.rs` - issue #541: detects a wedged backend TUI (one thread
  pinned ≥ 90% of one core in user-mode with near-zero process IO-write bytes,
  sustained for `DEFAULT_REQUIRED_STREAK` × `DEFAULT_TICK` ≈ 90 s). Pure
  `WedgeDetector` state machine (`Healthy` / `Suspect{streak}` / `Wedged`) is
  platform-free and exhaustively unit tested; the Windows-only sampler walks
  the monitored pid's process subtree via `Toolhelp32` + `GetThreadTimes` +
  `GetProcessIoCounters`. On `Wedged`, `WedgeWatchdog` (same
  `Drop`-joins-thread shape as `BannerWatcher` above) prints one rate-limited
  stderr warning naming the backend and a `codex resume`-style recovery
  hint, and logs the measured signature via `verbose_log`. Wired into
  `runner::run_plan_subprocess` and `runner::run_plan_pty`. No-op on
  non-Windows. E2E probes against real spinning threads live in
  `tests/integration/reaper/platform/windows/wedge_watchdog_e2e.rs` (ignored; run manually).
  Issue #709: a healthy tick no longer pays for the host-wide thread
  enumeration. `subtree_could_hide_a_hot_thread` compares each process's
  *user-mode* delta against `GATE_USER_PCT_THRESHOLD` first — a thread's user
  time can never exceed its process's, so a cool subtree provably has no hot
  thread and the `TH32CS_SNAPTHREAD` walk (which enumerates **every thread on
  the host** before filtering) is skipped. The gate fails open on any
  unanswerable input, and a gated tick still reports an explicitly *healthy*
  sample rather than `None` — returning `None` would leave a partial wedge
  streak standing across an idle stretch, since the loop treats it as "no
  observation". `descendants_of` also carries a visited set: a PID-reuse cycle
  in the Toolhelp parent graph previously made the walk non-terminating.
- `workspace_trust.rs` - issue #1102: reads
  `projects["<abs cwd>"].hasTrustDialogAccepted` out of Claude Code's
  `~/.claude.json` (or `$CLAUDE_CONFIG_DIR/.claude.json`) and returns
  `WorkspaceTrust::{Trusted, Untrusted, Unknown}` — `Unknown` for a missing or
  unparseable file, so a relocated config never warns a user that a trusted
  workspace is untrusted. `warn_if_workspace_untrusted` prints one stderr
  notice, called from `main.rs` above the `launch_mode` match so the legacy
  external-loop `clud grind` gets one line rather than one per iteration; it is silent
  unless the backend is Claude, the workspace is untrusted, **and** the repo
  actually has a `.claude/settings*.json` for that decision to suppress. Never
  writes the trust flag — see
  [DD-066](../../../docs/DESIGN_DECISIONS.md#dd-066-clud-reports-an-untrusted-workspace-and-never-records-the-trust-itself).
  Tests: `workspace_trust_tests.rs` (decision table + notice text),
  `tests/integration/test_workspace_trust_notice.py` (end-to-end, exactly-once).
- `stage_trace.rs` - `launch-stage` / `exit-stage` breadcrumbs written to
  `CLUD_EXIT_TIMING_FILE` (and to stderr under `--verbose` /
  `CLUD_EXIT_TIMING`) as each stage begins and ends, so the integration
  harness can name the stage a killed process was in (#594, #1168). Used by
  `main.rs` (exit stages, post-run drops) and `runner.rs` (`child_wait`,
  `child_teardown`, `runtime_drop`, `cpu_banner_stop`). See `docs/architecture/ci.md`.
- `verbose_log.rs` - launch-clock + opt-in file logging
  (`CLUD_VERBOSE_LOG_DIR`); `log()` writes timestamped lines to the per-launch
  log file.
- `crash_report.rs` - process panic hook + native crash handler installed
  from `main.rs` (role=`foreground`), `daemon/server.rs::run_daemon`
  (role=`daemon`), and `daemon/worker.rs::run_worker` (role=`worker`).
  Both panic-driven and native-crash-driven (`crash-handler` crate;
  SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGABRT on Unix; structured exceptions on
  Windows) reports share one writer producing JSON records with backtrace
  under `~/.clud/state/crashes/<unix_ms>-<role>-<pid>.json`, prunes to
  the 50 most recent, and surfaces a one-line stderr notice on the next
  launch when a new report appears (plus a follow-up "backtrace appears
  unsymbolicated; run `clud symbols verify`" line when the new report
  has zero `at FILE:LINE` frames — #374 PR 3). `install_native()` is
  idempotent — the hook is installed once per process; re-calling only
  updates the role tag. Native install **does not attach a
  SIGINT/CTRL_C_EVENT handler**, leaving the existing
  `startup::install_ctrl_c_flag` / `ctrl_c_track` (#372) path
  authoritative for Ctrl-C.
- `symbols.rs` - `clud symbols` / `clud symbols install` / `clud symbols
  verify [--all]` subcommand handler. `verify` confirms the running binary
  can resolve recent crash reports' `at FILE:LINE` frames and exits 0/1.
  `install` fetches a release's Linux `.dwp` only when the report's GNU
  build-id matches the release's `BUILD-IDS.txt`, verifies it against
  `SHA256SUMS`, and caches it under `~/.clud/state/symbols/<build-id>/`
  (2 newest kept); anything unpaired is a skip, not an error (#1016). The
  bare `clud symbols` form prints a five-line summary. Self-contained
  maintenance command; dispatched from `main.rs` before any backend
  resolution. See `docs/architecture/crash-reports.md`.
- `workflow/` - `clud workflow` script engine (QuickJS), `clud -p` agent runner and ASCII board; see [workflow.md](../../../docs/architecture/workflow.md).
- `wasm.rs` - `wasmi`-based runner that loads a WASM module, registers a
  minimal `host.log` import, invokes a named export, and propagates the integer
  exit code.
- `claude_files.rs` - installs the bundled `grind-*` agent types and `grind-run` workflow into `~/.claude/{agents,workflows}/` with the skill installer's `managed-by: clud` ownership rules.
- `civil_time.rs` - issue #1206: the crate's one civil-date algorithm.
  `civil_from_unix_secs(unix_secs: i64) -> (year, month, day, hour, minute,
  second)` is Howard Hinnant's public-domain `civil_from_days` plus a
  euclidean seconds-of-day split, so pre-epoch timestamps land on the
  previous day rather than truncating toward zero. It replaced four drifted
  copies (`loop_artifacts::unix_to_ymd_hms`,
  `command::loop_task::unix_to_ymd_hms`, `trash::civil_from_days`,
  `tool_query::days_to_string`). Scope is the calendar arithmetic **only**:
  each caller keeps its own `format!` string, because those strings are
  already baked into data on disk — `trash` names quarantine directories
  `YYYYMMDDTHHMMSSZ` and `loop_artifacts` writes ISO-8601 into persisted
  `info.json` / `log.txt`. Do not fold formatting into this module. No
  `chrono` / `time` dependency.

Quick lookup, which file owns a given subcommand:

- `clud doctor` -> `doctor::run`.
- `clud loop ...` -> `command::build_launch_plan_for_target` (resolved
  provider/harness, prompt, and markers) +
  `loop_spec` (task resolution) + `loop_artifacts` (artifact files) +
  `runner.rs` (iteration loop) + `loop_check` (DONE/BLOCKED scan).
- `clud ci [JOB] [--filter RE] [--json]` -> `ci_local::run` (local bosn run with a
  compact verdict and failing-step diagnostics, #1839).
- `clud dsh-update` -> `dsh_harness::run_update` (managed DeepSeek Harness
  install, #1829).
- `clud video [DIR] [--update]` -> `video.rs` (pinned video-use checkout,
  plugin wrapper, ffmpeg and ElevenLabs-key preconditions); `command/builder.rs`
  adds `--plugin-dir` and the seed prompt (#1851).
- `clud --detach`, `clud attach`, `clud list`, `clud kill`, `clud logs` -> all
  in `daemon/` (dispatched from `daemon::handle_special_command`).
- `clud gc list` / `prune` / `purge` / `all` / `reconcile` -> `gc/cli.rs` (CLI handlers) talking to
  `daemon/gc_service.rs` (registry owner inside the always-on `__daemon`).
- `clud grind [url]` -> `grind.rs` resolves the repo's `origin` remote to its
  forge issues page (GitHub `<repo>/issues`, GitLab `<repo>/-/issues`), prints
  the green notice, and keeps the `Grind` command (issue #897 — it no longer
  rewrites to `Do`). `command/builder.rs` builds the `/loop` seed prompt.
  The intended contract is one ordinary interactive PTY prompt, with the
  Claude harness owning repetition; it has no clud DONE/BLOCKED markers,
  relaunches, headless path, or iteration ceiling. Other harnesses fail before
  launch. See the [grind architecture](../../../docs/architecture/grind.md). An explicit URL
  is passed through verbatim.
- `clud grind reconcile` -> `grind_reconcile.rs` (dispatched in `main.rs` via
  `args::is_reconcile`): the feature-branch repair pass of #1393, reading only
  GitHub. It is not a launch — no harness is spawned — so it is exempt from the
  Claude-harness requirement `grind_launch_error` applies to the interactive
  `/grind` launch (#1803); the gate keys that exemption on the same
  `args::is_reconcile` predicate. See
  [grind.md](../../../docs/architecture/grind.md#never-losing-issues-in-feature-mode).
- `clud grind-facts path|clear|task|owner` -> `grind_facts.rs` (dispatched early in
  `main.rs`): this session's `/grind` run-facts file,
  `~/.clud/tmp/grind/<session_id>.json`, which the hook's grind caps read by
  the payload's `session_id` (#1337); see
  [grind.md](../../../docs/architecture/grind.md#router-questions).
- `clud --clean-worktrees` -> `worktrees.rs`.
- `clud optimize rust` -> `optimize.rs`.
- `clud --fix-hooks` -> `hook_health/`.
- `clud settings [--list]` -> `settings_tui.rs`.
- `clud settings set|roots` -> `settings_change.rs`, with native approval in
  `settings_change/gui.rs` and operation tests in `settings_change/tests.rs`.
  Authorization contract: [rm-tools.md](../../../docs/architecture/rm-tools.md#user-approval-for-settings-changes-1999).
- `clud extern trust <name> [--list|--revoke]` -> `extern_cli.rs` (+
  `hook_trust.rs` for the store).

## Cross-Cutting Subsystems

Subsystems that span multiple files have their own topic docs under
`docs/architecture/`:

- **Loop subsystem** (`command/`, `loop_spec`, `loop_check`, `loop_artifacts`,
  `stream_json`, `runner`) -> [docs/architecture/loop-subsystem.md](../../../docs/architecture/loop-subsystem.md)
- **Grind** (`grind`, `command`, `runner`) ->
  [docs/architecture/grind.md](../../../docs/architecture/grind.md) — intended
  one-PTY `/loop` handoff and the current implementation mismatch.
- **Daemon IPC** (everything under `daemon/`) -> [docs/architecture/daemon-ipc.md](../../../docs/architecture/daemon-ipc.md)
- **Session lifecycle** (`session`, `console_*`, `capture`, `dnd` injection,
  `voice` hooks) -> [docs/architecture/session-lifecycle.md](../../../docs/architecture/session-lifecycle.md)
- **Skill system** (`skills`, `assets/skills/`) -> [docs/architecture/skill-system.md](../../../docs/architecture/skill-system.md)
- **Launch setup** (`launch_setup`, selected-backend persistent setup) -> [docs/architecture/launch-setup.md](../../../docs/architecture/launch-setup.md)
- **GC and registry** (`gc`, `daemon/gc_service`, `session_registry`,
  `worktrees`) -> [docs/architecture/gc-and-registry.md](../../../docs/architecture/gc-and-registry.md)
- **Windows quirks** (`trampoline`, `subprocess` BatBadBat, `console_*`,
  foreground Job Object shell-orphan reaping, `dnd`, `win_creation_flags`,
  `voice` ARM carveout) -> [docs/architecture/windows-quirks.md](../../../docs/architecture/windows-quirks.md)
- **Launch plan** (`command/types::LaunchPlan` + all consumers) -> [docs/architecture/launch-plan.md](../../../docs/architecture/launch-plan.md)
- **Launch targets** (provider/harness resolution + sticky settings) -> [docs/architecture/launch-targets.md](../../../docs/architecture/launch-targets.md)
- **Provider selection** (routing mode + provider-neutral model registry) -> [docs/architecture/provider-selection.md](../../../docs/architecture/provider-selection.md)
- **Unified gateway** (`codex_bridge`, `codex_history`, `foreground_runtime`,
  `provider_catalog`, `auth`) -> [docs/architecture/unified-gateway.md](../../../docs/architecture/unified-gateway.md)
- **Hook dispatch** (`clud_hooks*`, `clud_hook_roots`, `hook_trust`,
  `extern_cli`, `block_bad_cmd*`, `extern_root`, `hook_health`) ->
  [docs/architecture/hook-dispatch.md](../../../docs/architecture/hook-dispatch.md)

Non-obvious design choices (single `LaunchPlan`, `lib.rs` as the only
`mod ...` site, cooperative Ctrl+C, redb single-owner) have ADRs in
[docs/DESIGN_DECISIONS.md](../../../docs/DESIGN_DECISIONS.md).

## Entry Point

`main.rs` is the binary entry; `lib.rs` re-exports every top-level module (and
the four subdirs) as `pub mod ...` so integration tests under
`crates/clud-bin/tests/` can link against internals. See
[DD-007](../../../docs/DESIGN_DECISIONS.md#dd-007-librs-is-the-only-place-that-declares-modules-mainrs-imports-through-clud)
for why the single-instantiation pattern matters.

## See Also

- Parent crate overview: [`../README.md`](../README.md).
- Top-level project docs and CI matrix: [`../../../CLAUDE.md`](../../../CLAUDE.md).

Session rm activation and hook identity checks: [rm protection](../../../docs/architecture/rm-protection.md).
Agent deletion (`safe-rm`, trash, roots, the hook redirect): [rm tools](../../../docs/architecture/rm-tools.md).

- `rm_tool.rs` — `safe-rm` (#1340): flags, `Roots` from `CLUD_RM_ROOTS` or the checkout (plus its worktrees), `resolve`'s refusals, the system temp roots (#1622, #1659 `/dev/shm`; RAM-backed ones purge instead of trashing, [rm-tools.md](../../../docs/architecture/rm-tools.md#system-temp-directories-1622)), trash entries with `.clud-rm.json`, `--purge`, the per-call audit log, and `session_roots_value` for the child env; tests in `rm_tool_tests.rs`.
- `rm_tool_ledger.rs` — submodule `rm_tool::ledger`: the creation-ledger consult for a path outside every root (#1666), as `verdict` over injected `LedgerFacts` plus `probe` and the daemon-backed `DaemonLedger`; tests in `rm_tool_ledger_tests.rs`; policy in [rm-tools.md](../../../docs/architecture/rm-tools.md#creation-ledger).
- `safe_mktemp.rs` — the `safe-mktemp` multicall name (#1667): exclusive `mkdir` of one directory, then the only creation-ledger insert (`gc_client_insert_created`); undoes the `mkdir` if the insert fails, refuses on Windows; tests in `safe_mktemp_tests.rs`; policy in [rm-tools.md](../../../docs/architecture/rm-tools.md#creation-ledger), [DD-136](../../../docs/DESIGN_DECISIONS.md#dd-136-safe-mktemp-is-the-only-ledger-writer-and-undoes-its-mkdir-when-the-insert-fails).
- `rm_tool_clone.rs` — submodule of `rm_tool.rs`: whether a clone outside the roots may be removed (#1573), as `verdict` over injected `CloneFacts` (origin match, clean, fully pushed, no stash) plus the git `probe`; rule in [rm-tools.md](../../../docs/architecture/rm-tools.md#location-policy).
- `rm_tool_extra_roots.rs` — submodule `rm_tool::extra_roots`: the human-set `safe_rm.extra_roots` override (#1668), as `verdict` over injected `EntryFacts` plus `probe`/`load`; the settings reader is `clud_settings::safe_rm_extra_roots_from`; tests in `rm_tool_extra_roots_tests.rs`; policy in [rm-tools.md](../../../docs/architecture/rm-tools.md#override-safe_rmextra_roots-1668), [DD-137](../../../docs/DESIGN_DECISIONS.md#dd-137-the-safe-rm-root-override-is-user-level-only-and-agents-cannot-write-the-settings-file).
- `rm_tool_home_files.rs` — submodule `rm_tool::home_files`: paths under `$HOME` outside the roots (#1956, #1957): single files only inside `FILE_ROOTS`, directories only inside `DIR_ROOTS`; `PERSONAL` folders and `DENYLIST` entries refused; tests in `rm_tool_home_files_tests.rs`; policy in [rm-tools.md](../../../docs/architecture/rm-tools.md#paths-under-home-1956-1957).
- `rm_tool_session_grants.rs` — submodule `rm_tool::session_grants`: explicit session-scoped grants for sibling Git checkouts (#1784), with reasoned audit records and Unix checkout identity checks; tests in `rm_tool_session_grants_tests.rs`; policy in [rm-tools.md](../../../docs/architecture/rm-tools.md#authorized-sibling-checkouts-1784), [DD-157](../../../docs/DESIGN_DECISIONS.md#dd-157-sibling-checkout-cleanup-uses-an-explicit-session-grant).
- `rm_tool_own_trash.rs` — submodule `rm_tool::own_trash`: a session may `safe-rm --purge` the trash entries it created (manifest or `clud trash` owner marker names its session id, #2096); a non-purge call on them is refused; tests in `rm_tool_own_trash_tests.rs`.
- `block_bad_cmd_claude_worktrees.rs` — refuses creating `.claude/worktrees/` or a new entry in it, in any repo or the home directory, from the shell (read from the syntax tree) or the file tools; work inside an existing checkout there and cleanup stay allowed (#1962, #1971). Tests in `block_bad_cmd_claude_worktrees_tests.rs`.
- `claude_worktree_hook.rs` — the `WorktreeCreate` / `WorktreeRemove` handlers (`clud-cmd-scan --event WorktreeCreate`): Claude Code's own worktrees are built under `~/.clud/worktrees` instead of `.claude/worktrees/`; registered for Claude Code 2.1.50+ by `clud_hooks_compile::worktree_hooks_fragment`. See [shell-ast.md](../../../docs/architecture/shell-ast.md#worktree-location-guards).
- `worktree_consent.rs` — #1984: worktree creation needs the user's consent (prompt → daemon session flag, `CLUD_ALLOW_WORKTREES`, `--worktree`, an active `/grind` run); asked by `block_bad_cmd` and the `WorktreeCreate` handler. See [session-controls.md](../../../docs/architecture/session-controls.md).
- `block_bad_cmd_shell.rs` — the scanner's bridge to `shell_ast` (#1962): `parse_command` (seeded with the hook's environment and the payload cwd) and `rule_words` (argv with only `env`/`command`/`exec` unwrapped); no rule splits command text by hand. Scanner-level tests for capture and the location guards are in `block_bad_cmd_ast_tests.rs`.
- `block_bad_cmd_rm_redirect.rs` — the hook's redirect of an agent's own `rm`/`rmdir`/`unlink`/`find -delete`/`xargs rm` to safe-rm, and `rm_tool_only`, the no-prompt allow.
- `block_bad_cmd_wait_loop.rs` — the hand-written wait guard (#1880): refuses `while`/`until`, sleeping `for` loops, `tail -f`, `watch` and `sleep` chained to a `gh` status read, naming verified process-session, background or bounded-foreground capabilities as the replacement (#2004); tests in `block_bad_cmd_wait_loop_tests.rs`; contract in [hook-dispatch.md](../../../docs/architecture/hook-dispatch.md#hand-written-wait-guard-1880), [DD-168](../../../docs/DESIGN_DECISIONS.md#dd-168-hand-written-waits-are-refused-a-short-standalone-sleep-is-not).
- `block_bad_cmd_wait_scan.rs` — #1972 helpers for the wait guard: peels wrapper programs (`timeout`, `nohup`, `env`, `xargs`, …), extracts `-c`/`eval`/`python -c` payloads, heredoc scripts and literal script files, and judges Python poll loops; tests and the audit corpus runner in `block_bad_cmd_wait_scan_tests.rs` (fixture `../tests/fixtures/wait_loop/corpus.json`); [DD-169](../../../docs/DESIGN_DECISIONS.md#dd-169-a-wrapper-an-embedded-script-or-another-tool-does-not-launder-a-wait).
- `block_bad_cmd_repeat.rs` — the repeated-identical-call guard (#1674): `decide` over an injected `Streak`, per-session hash-only state under `<state>/repeat-guard/`, fail-open; tests in `block_bad_cmd_repeat_tests.rs`; contract in [hook-dispatch.md](../../../docs/architecture/hook-dispatch.md#repeated-call-guard-1674), [DD-140](../../../docs/DESIGN_DECISIONS.md#dd-140-the-repeated-call-guard-denies-call-201-of-an-identical-streak-and-fails-open).
- `block_bad_cmd_loop_stop.rs` — the `/loop` first-round stop guard (#1949): on a non-Claude route (`CLUD_ROUTE_CONTEXT.model_provider`), refuses `ScheduleWakeup {"stop": true}` once when the transcript shows a `/loop` that never scheduled a wakeup; a repeated stop in the same round, a loop that has scheduled before, or an unreadable transcript all pass. The transcript is read only for that one call. The harness delivers `ScheduleWakeup` only because `clud_hooks_compile::deletion_safety_fragment` lists it in `GUARDED_TOOLS`, the one matcher that names every tool a scanner rule inspects.
- `block_bad_cmd_rm_override.rs` — refuses any agent shell command that could write `~/.clud/settings.json` or the `safe_rm.extra_roots` key (#1668), before the per-call opt-out.
- `rm_guard.rs` — catastrophe-floor decisions, canonical operands, and mount
  protection; it contains no removal implementation. In a session,
  `shim_main.rs` hands accepted requests to the next real `rm` on PATH.
- `deletion_policy.rs` — normalized deletion-base semantics shared by source
  interpretation and the shim.
- `block_bad_cmd_rm_identity.rs` — effective-PATH byte identity and source
  resolution checks, shared by both hook entrypoints.
- `doctor.rs` — `clud doctor [--json]` (#1954): reports stale, missing and retired aliases in clud's interpreter, session and helper alias dirs (`inspect_dir`), lists non-alias files there without deleting them, flags a foreign first `clud-cmd-scan` on PATH, and computes scanner coverage of the launch fragment and of each hook config on disk (`coverage_report`); exit 1 on an alias finding; tests in `doctor_tests.rs`. See [hook-dispatch.md](../../../docs/architecture/hook-dispatch.md#hook-health-clud-doctor).
- `hook_handshake.rs` — the hook build handshake (#1954): startup exports the launcher path and build fingerprint; `multicall::run_cmd_scan` relays a skewed `clud-cmd-scan` to `<launcher> __cmd-scan` and logs `HOOK-SKEW`; decision is `decide` over injected `Facts`; tests in `hook_handshake_tests.rs`; see [launch-setup.md](../../../docs/architecture/launch-setup.md).
- `multicall.rs` — argv[0] dispatch, the first thing `main` does (#1551,
  DD-121): `clud-cmd-scan`/`clud-block-bad-cmd` → `block_bad_cmd::run`,
  `clud-shim` and every `shim_registry::SHIMS` name → `shim_main::run`, plus the
  hidden `clud __cmd-scan`, `clud __shim <name>` and `clud __link-aliases <dir>`.
  `alias_link.rs` creates an alias: hardlink, then symlink, then copy, never
  failing a launch, with a freshness check that relinks after an upgrade.
- `shim_main.rs` also owns the session-local GitHub CLI relay and PR-watch translation;
  see [gh-watch-shim.md](../../../docs/architecture/gh-watch-shim.md).
- `gh_broker/` — the session `gh api` read broker (#1743): `classify.rs`
  (which argv is a brokerable read or a possible write), `client.rs` (shim
  side and loopback replay), `service.rs` (daemon side: TTL, single-flight,
  304 revalidation, ledger), `service/merged.rs` (merged collection reads:
  every page revalidated by ETag), `collection.rs` (plans, pages, merge),
  `scope.rs` (targeted invalidation tags), `store.rs` (redb), `upstream.rs`
  (`gh api -i`).
  See [gh-read-broker.md](../../../docs/architecture/gh-read-broker.md).
- `shim_telemetry.rs` — the per-invocation JSONL record of the in-session
  `git` / `gh` pass-through (#1486), `<state>/logs/shim/git-gh.jsonl`,
  rotated at 8 MiB; `shim_main.rs::run_child` runs the child and records.
  See [git-gh-telemetry-shim.md](../../../docs/architecture/git-gh-telemetry-shim.md).
- `shim_registry.rs` — `SHIMS`, the one list of `clud-shim` aliases, plus the
  session key names, ABI stamp and the PATH resolver every alias shares.
  `shim_main/dispatch.rs` is the shim personality's single entry path: session
  checks, target resolution and fail-open passthrough (#1546); see
  [shim-dispatch.md](../../../docs/architecture/shim-dispatch.md).

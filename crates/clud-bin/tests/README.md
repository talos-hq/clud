# tests/

Rust integration tests for the `clud-bin` crate. Unlike the `#[test]` units inside `src/`, these tests spawn the workspace `mock-agent` binary through `running_process_core::pty::NativePtyProcess` and exercise the real PTY pump used by `clud --codex`. They lock down platform-specific contracts (Windows ConPTY vs POSIX), cross-platform regressions from issues #28/#31/#46, and the voice/F3 + resize hooks implemented in `clud::session`. A `pty_canary()` probe runs first and skips the test if the host's stdout isn't a real console (typical in nested shells or captured `cargo test` runs).

This harness is **Tier 1**: it spawns a built binary, so `bash test --host`
skips it ([test tiers](../../../docs/architecture/ci.md#two-local-test-tiers-2356)).

## Layout: one harness (#1726)

Everything here compiles into **one** test binary, `integration`
(`integration/main.rs`), with one module per category: `api/`, `cli/`,
`diagnostics/`, `pty/`, `reaper/`, `signals/`, plus the shared `common/`.
Test IDs are `<category>::<module>::<test_name>`. Each harness links the whole
workspace and zccache never caches harness link products, so a new test is a
module in a category, never a new `tests/*.rs` or `tests/<dir>/main.rs`
target: `ci/harness_budget.py` fails `bash lint` on one. CI
(`ci/run_bundle.py`, `ci/harness_plan.py`) still runs each category in its
own process and each `pty::` test in its own pseudo-terminal.

Host-specific code follows the platform boundary (#1953): no host `cfg` or
native API crate outside a `platform/` directory. Runtime host branches use
`clud::platform::host::*`. Code that must name OS APIs, and whole host-only
modules, live in the category's own boundary,
`<category>/platform/{mod.rs,unix/,windows/}`, keeping their basenames
(`ci/banned_imports.py` exempts by basename). Their test IDs gain a
`platform::<host>::` segment, e.g.
`reaper::platform::windows::wedge_watchdog_e2e::...`.

## Files

- `pty_behavior.rs` — End-to-end PTY checks: `respond_to_queries_impl` DSR stub (T1), `resize_impl` propagate-on-POSIX / no-op-on-Windows (T2), `session::resize_pty` updating the master size on every platform (`resize_pty_updates_master_size_on_all_platforms`, moved from the `--lib` tests by #1348 so it cannot silently skip), extreme `cols=32767` spawn safety (T3), verbatim stdin forwarding through `run_raw_pty_pump`, F3 press/release detection (xterm + kitty CSI-u), idle `on_tick` cadence, Ctrl-C flag honoring, resize channel application, prompt exit on child death, and raw-mode recovery on hook panic.
- `pty_pump.rs` — Raw PTY pump contracts: verbatim stdin forwarding (#46), voice F3 press/release hooks (#13/#41), idle ticks, Ctrl-C/interrupt propagation (flag + extra_rx 0x03), resize-channel delivery, prompt exit on child death, hook-panic recovery, Shift+Enter extra_rx round trip (#141), the terminal modes a Ctrl+C-interrupted child left on being undone (#1704), and the issue #538 writer-thread decoupling (`stdin_forwarding_stays_fast_while_output_sink_stalls`, via the `run_raw_pty_pump_full_with_writer_for_test` sink-injection seam).
- `pty/input_corpus.rs` — End-to-end input fidelity sweep (#1697 follow-up): a corpus of what real terminals send (SGR mouse, arrows/function keys, query replies, kitty CSI-u keys, Alt+key, bracketed paste, UTF-8, emoji) goes through both production input paths (the `extra_rx` console channel and the byte-stream stdin reader), whole, one byte per write, and in 3-byte strides, into `mock-agent` on a real PTY; the child must read it byte-for-byte (`through_pty_input` models ConPTY's LF→CR).
- `pty/toast_pty.rs` — #1189 toasts and #1359 transient CPU HUD in real PTY sessions through the composited writer (`run_raw_pty_pump_with_toasts_for_test`): the Kitty tier uploads, fades, removes, and frees the CPU image during a quiet child interval with `q=2` while child text replays intact; the title fallback pushes/pops the title stack (Linux, macOS, Windows); the alternate-screen text tier repaints exactly what the child drew (Unix).
- `pty/screen_fidelity.rs` — #1942 screen fidelity. A fullscreen alternate-screen frame, a resize through the pump's resize channel, and an incremental repaint that relies on earlier cells. clud's output, replayed into a terminal model, must match the child's intended screen cell by cell (text and background). The test runs on every platform, which is what makes it meaningful on the Windows ConPTY lane.
- `pty/platform/windows/start_terminal.rs` — #2007, Windows only. Bare `start cmd` and `start cmd.exe` requests open a console that survives PTY close and foreground cleanup; argument-bearing forms retain stock dispatch. The fixture verifies the persistence marker and cleans up using process identities.
- `pty/platform/windows/console_wrap.rs` — #1942, Windows only. During a PTY session the stdout console defers its end-of-line wrap (`DISABLE_NEWLINE_AUTO_RETURN`): a full-width bottom row leaves the cursor pending in the last column and does not scroll the screen, as a relayed fullscreen TUI assumes. The guard restores the original mode on drop.
- `pty/platform/windows/shift_enter_dual_reader.rs` — Windows native terminal-input consumer regression: sends native key records through running-process's translator, verifies complete arrow/Home/End/Insert/Delete/Page sequences and trace bytes (#575), and exercises the real `WriteConsoleInputW` → `TerminalInputCore` → clud policy-adapter path when a console is attached; also preserves Shift+Enter's literal-LF contract (#141).
- `diagnostics/platform/windows/win32_hooking_probe.rs` — Ignored Windows-only #468 research probe for raw Job Object lifecycle events, PEB command-line reads, handle snapshots, breakaway denial, and LoadLibrary DLL injection against `testbins/probe-*`.
- `reaper/platform/windows/tool_shell_lifecycle_windows.rs` — Windows-only #616 live Job completion-port coverage: a registered fake agent's direct leaked client is reaped while an intentionally detached nested `cmd.exe` survives until explicit cleanup, and (#673 Phase 5) the session's reap summary and `reap.jsonl` name the kill.
- `reaper/platform/windows/reaper_daemon_survival_windows.rs` — Windows-only #674 daemon-survival coverage against a real Job Object: all three real detach shapes survive a tool-shell lifecycle, a genuinely leaked client is still reaped, and one session's exit does not reach another session's daemon. Uses `testbins/daemon-stub`, never a real sccache/docker install.
- `reaper/platform/windows/reaper_batch_drain_windows.rs` — Windows-only #706 completion-port drain coverage: a 48-process burst queues notifications faster than the listener consumes them, proving the drain folds them into batches that share **one** host process-table enumeration (measured 5 scans, not ~48). The folding rule itself is Tier 1 (`job_orphan_reaper::batch_tests`); only the real port's queuing behaviour needs a real Job Object. Prints its measurement under `--nocapture`.
- `diagnostics/platform/windows/runtime_cache_hop_windows.rs` - Windows-only #333 regression: the runtime-cache relay must leave a real detached daemon alive after the relay exits, proving it adds no `KILL_ON_JOB_CLOSE` containment.
- `reaper/platform/windows/wedge_watchdog_e2e.rs` — Ignored Windows-only #541 end-to-end probe for the wedge watchdog (`clud::wedge_watchdog`). Drives the real Toolhelp32/`GetThreadTimes`/`GetProcessIoCounters` sampler through the public `WedgeWatchdog` API against real spinning threads in the test process itself (no separate testbin needed — the sampler walks the subtree rooted at whatever pid it's given). Covers all three acceptance-criterion shapes: quiet single-thread spin reaches `Wedged`, spin-with-concurrent-IO stays `Healthy`, and spread multi-thread load stays `Healthy`.

## Where a reaper test goes

**Prefer unit tests. They are faster and they run on every platform.** An
integration test is justified only when the behaviour cannot be expressed
against injected process facts — in practice, only when a real Job Object or
real process detachment is the thing under test. Reap decisions take OS facts
as data (`ProcessFacts` / `FactsSnapshot`), so the decision table belongs in
`job_orphan_reaper`'s `lifecycle_tests` as a Tier 1 unit case asserting **spare
+ reason**, not just the outcome.

Integration coverage here has a stated budget of **≤5 tests**; anything
expressible in Tier 1 must be in Tier 1. Full tier rules, the daemon signal
table and the cooperative-marker caveat:
[`docs/architecture/process-reaping.md`](../../../docs/architecture/process-reaping.md).

## How to run

From the repo root:

```bash
bash test --host                            # Tier 0: library tests only, no binary (#2356)
bash test                                   # Rust + Python unit tests
bash test --integration                     # adds mock-agent integration tests
soldr cargo test -p clud-bin                # all clud-bin tests (unit + integration)
soldr cargo test -p clud --test integration pty::pty_behavior   # one module
soldr cargo test -p clud --test integration pty:: -- --nocapture   # see canary-skip diagnostics
```

Ignored Windows probes (`wedge_watchdog_e2e`, `win32_hooking_probe`, `tier_refresh_probe`): see the manual checklist in [`docs/architecture/ci.md`](../../../docs/architecture/ci.md#manual-windows-probes-ignored-tests).

All `cargo`/`rustc`/`rustfmt` invocations must go through `soldr` (see root `CLAUDE.md`). The mock-agent is auto-built on first run via `cargo build -p mock-agent --message-format json`.
- `tier_refresh_probe.rs` � Ignored #687 premise probe. Times `ProcessesToUpdate::All` against `Some([pid])` and against clud's direct `OpenProcess`/`GetProcessTimes` path. **On Windows a targeted sysinfo refresh is not cheaper than the host walk** (ratio ~1.0), while the direct per-PID call is ~4 orders of magnitude cheaper � which is why #687's `T1 / Pids(set)` targets must split into *identity* (direct OS call) versus *topology* (needs `parent_pid`, hence the daemon service). Platform-specific: re-run before trusting the conclusion on Linux/macOS.

Loop dashboard acceptance lives in `integration/pty/loop_console.rs`: real PTYs
through Bash, cmd and PowerShell cover message queueing, confirmed restart and
terminal restoration. See [loop dashboard](../../../docs/architecture/loop-subsystem.md#interactive-loop-dashboard).

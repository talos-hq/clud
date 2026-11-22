---
name: terminal-pty-linux
description: Use before changing or debugging clud's terminal/PTY path on Linux (POSIX PTY, byte-stream stdin, crossterm raw mode and OPOST, SIGWINCH resize, SIGINT/SIGTERM/SIGHUP handling, escape sequences, terminal restore). Covers the files to read, the invariants, full local validation through bosn act, and clippy pitfalls specific to libc types.
---

# Terminal / PTY on Linux

Shared facts (data flow, the three invariants, test tiers, act rules, CI
labels) are in
[docs/architecture/terminal-pty.md](../../../docs/architecture/terminal-pty.md).
Read that first. This page lists only what differs on Linux. macOS shares
this POSIX code; see [terminal-pty-macos](../terminal-pty-macos/SKILL.md).

## What is different on POSIX

- **PTY.** The PTY is running-process's `NativePtyProcess`. Never spawn
  through `std::process` (see CLAUDE.md).
- **Stdin.** Input comes from a byte-stream stdin reader, not a console
  event reader. There is no `extra_rx` keyboard source, and no Backspace or
  Shift+Enter rewriting.
- **Raw mode** is crossterm's `enable_raw_mode` inside
  `RawTerminalGuard::enter`, which clears `OPOST`. With `OPOST` cleared, a
  bare `\n` moves down without returning to column 0, so text walks
  diagonally. Never hand-roll raw mode or terminal writes in a selector; use
  `selector::run`
  ([DD-073](../../../docs/DESIGN_DECISIONS.md#dd-073-every-inline-selector-renders-through-one-component),
  guard test `migrated_selectors_never_drive_the_terminal_themselves`,
  `crates/clud-bin/src/selector.rs:994`).
- **Resize.** A `signal-hook` SIGWINCH watcher (`spawn_os_resize_watcher`,
  `session.rs:901`) feeds the pump's resize channel. `resize_impl` works on
  POSIX.
- **Interrupt.** `interrupt_pty_process` (`session/interrupt.rs:26`) first
  tries a daemon handoff. Otherwise it sends SIGINT to the child's
  foreground process group, signals the child's tree directly, waits up to
  2 s, then closes the PTY. It returns 130.
- **Other signals.** SIGTERM, SIGHUP and SIGQUIT are turned into the same
  cooperative `interrupted` flag (#517, `startup.rs:224`). The pump then
  exits normally, so `RawTerminalGuard::drop` restores the terminal.
- **No startup cursor query.** POSIX PTYs send no `ESC[6n`, so the local
  pump never stub-answers queries (`should_answer_cursor_queries` is false
  off Windows).
- **ConPTY behaviour does not apply here.** The kernel PTY passes the child's
  mode sequences through unchanged. The #1704 test therefore asserts the
  full restore set on Unix: scroll region, `?1`, `?1003`, `?1006`, `?1004`,
  `?1049`, `?2004` and `?25`.

## Files to read first

- `crates/clud-bin/src/session.rs`:
  - `RawTerminalGuard` (`:316`, `Drop` at `:602`)
  - `KeyboardEnhancementTracker` (`:344`)
  - `CHILD_TERMINAL_MODES_RESET` (`:453`)
  - `filter_user_input_chunk` (`:1197`)
  - `forward_user_input` (`:1252`)
  - the pump (`run_raw_pty_pump_full_verbose_with_writer`, `:1359`)
- `crates/clud-bin/src/session_stdin.rs`: `InterruptScanner` (`:65`).
- `crates/clud-bin/src/session/escape_gate.rs`,
  `crates/clud-bin/src/session/bracketed_paste.rs` and
  `crates/clud-bin/src/session/child_modes.rs`.
- `crates/clud-bin/src/terminal_queries.rs`: shared with the daemon worker.
- `crates/clud-bin/src/startup.rs:224`: the signal-to-flag bridge.
- [session-lifecycle.md](../../../docs/architecture/session-lifecycle.md),
  "The pump loop", "Shutdown" and "Terminal restore".

## Invariants you must not break

1. **All user input goes through `forward_user_input`.** The escape-sequence
   gate is enforced on every platform, even though the Linux kernel PTY
   does not flush a sequence split across writes.
2. **Stream scanners resume across reads.** Add a split-at-every-offset unit
   test for any new parser, for example `session_tests.rs:399` and
   `session_tests.rs:745`.
3. **Restore runs from `Drop`.** Keep the paths that end a session (child
   exit, Ctrl+C, signals turned into the flag, panic) ending in a normal
   return or an unwind. Do not `std::process::exit` from inside a session.
   Forced kills are out of scope
   ([DD-147](../../../docs/DESIGN_DECISIONS.md#dd-147-ctrlc-restores-the-terminal-in-process-forced-kills-are-out-of-scope)).
4. **Never push `DISAMBIGUATE_ESCAPE_CODES`.** With it set, Ctrl+C arrives
   as CSI u instead of `0x03`, and raw mode has already cleared `ISIG`
   (#1101; see session-lifecycle.md, "Startup").

## How to validate a change

Shared business logic and callers of unchanged platform APIs use routine
Linux validation. A change to the POSIX platform implementation itself
(termios, native PTY APIs, signals or OS contracts) selects affected native
lanes using [CI label selection](../../../docs/architecture/ci.md#when-to-add-native-ci-labels).
Do not infer a macOS/Windows change from sharing `session.rs` or extracting
helpers. Linux-only implementation changes need no full matrix.

Linux is the one platform agents can fully validate locally:

```bash
soldr lint --host-only   # host lint: fmt, Clippy, Dylint, then the static checks (never under bosn)
bosn ci run --workspace . --trigger pr --job test-linux-x64-unit --wait
uvx --from git+https://github.com/zackees/ci.yml@98fd662a48df43fe016c45a1f845e345b52fecef ci-lint local-gate run   # host lint lanes + bosn unit replay + stamp
```

- Never run host `cargo` or `bash test`; lint runs on the host only through `soldr lint`.
- Read terminal-pty.md § Validation for what a local run covers.
- openpty-based tests work in act2's job containers.
- Routine PRs run Linux x64 on GitHub. The `ci-test` label adds Linux
  integration.
- Watch a PR with
  `clud tool run github/pr_merge_watch.py -- <PR> [--no-cancel]`.

A test that needs raw `std::process::Command` needs a filename exemption in
`ci/banned_imports.py`. Add one only when `NativeProcess` would change the
behaviour under test.

## Known behaviours and gotchas

- **Clippy runs on tests too, with `-D warnings`.**
  - In #1709, `dead_code` on an unread field of a test struct failed CI.
    Prefix the field with `_`.
  - Lints like `clippy::manual_is_multiple_of` apply to test code as well.
- **libc types differ between platforms.** On Linux, `libc::tcflag_t` is
  `u32` and `cc_t` is `u8`; on macOS, `tcflag_t` is `u64`. An `as` cast to
  the same type trips `clippy::unnecessary_cast` on one platform or the
  other. Prefer `From`, `try_from`, or slice copies.
- **Mouse garbage in the shell after exit (#1383, #1701)** means a mode was
  left on. Check that the bytes went through
  `KeyboardEnhancementTracker::observe` before the reader was joined.
- **A query split across reads went unanswered (#1702).** The pump only
  answers queries when stdin is not interactive, which is Windows only.
  However, the daemon worker answers on every platform while no client is
  attached (`detached_query_replies`, `daemon/worker.rs:669`).

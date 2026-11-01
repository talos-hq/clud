# Windows Quirks

> Terminal/PTY behaviour shared across platforms (input pipeline, escape
> sequence atomicity, terminal restore) lives in
> [terminal-pty.md](terminal-pty.md); the agent playbook for Windows is the
> `terminal-pty-windows` skill (`.claude/skills/terminal-pty-windows/`).

This doc is the inventory of every place `clud` has Windows-specific code,
with the symptom each piece solves and the `file:line` where it lives. There
are eleven such carve-outs today: a self-rename trampoline so `pip install`
can overwrite a running `clud.exe`, the BatBadBat `.cmd`/`.bat` rewrite
mandated by Rust 1.77+, an RAII guard for `ENABLE_VIRTUAL_TERMINAL_INPUT`, a
small policy adapter over running-process's native `ReadConsoleInputW`
translator, a console-title keeper that re-stamps `clud <cwd>` when child TUIs
overwrite it, an OLE `IDropTarget` adapter so dragging a file onto the
console window actually drops paths into the prompt, `CREATE_NO_WINDOW` for
daemon-helper subprocesses that would otherwise flash a conhost window, a
Ctrl+C descendant-tree teardown that reaps orphaned backend grandchildren
without tripping cmd.exe's batch-job prompt, a Codex `PreToolUse` hook
diagnostic for batch wrappers that do not propagate `$LASTEXITCODE`, a
Claude Code hook stdin diagnostic for the Windows pipe/TTY bug cluster, and
foreground tool-shell lifecycle tracking. All eleven degrade to no-ops (or
different mechanisms entirely) on POSIX. (The former `whisper-rs` ARM
carve-out was removed along with the `whisper-rs` dependency entirely —
voice transcription is stubbed on every platform now; see
`crates/clud-bin/src/voice/README.md`.)

## Git Bash / mintty

Git Bash's mintty terminal is not a Windows console: without `winpty` a
native `clud.exe` gets pipe stdio, so `session::terminals_are_interactive()`
is false and clud runs in subprocess mode (pipes cannot do raw mode or
ConPTY). This is deliberate, not fixed by lying about the TTY (#1357).
Instead `session::warn_if_mintty_without_console()` (called once from
`main.rs` on the launch path) detects Windows + non-terminal stdin/stdout +
a real `TERM` + non-empty `MSYSTEM` and prints one stderr line explaining the
downgrade. Set `CLUD_NO_MINTTY_WARNING=1` to silence it. Workaround: run clud
from Windows Terminal, or wrap it as `winpty clud ...`.

## Why so many?

Each of these is individually small. Cumulatively they exist because Windows
differs from POSIX in six distinct ways `clud` cares about: ConPTY
semantics are not VT100 (so we have to opt into virtual-terminal input and
re-translate console input records); COM (`IDropTarget`, `OleInitialize`)
is the only supported integration point for drag-and-drop into a console
window; running executables are file-locked, so the standard
`pip install --force-reinstall` overwrite path silently fails; cmd.exe's
command-line parser is idiosyncratic enough that Rust's stdlib
(post-CVE-2024-24576) refuses to launch a `.cmd` directly; PowerShell and
batch-wrapper exit-code propagation can mask a failed native hook; and the
console process group sends `CTRL_C_EVENT` to every attached process, including
grandchildren we don't directly control. None of these are bugs in `clud`;
they are platform contracts that we absorb in one module each so the rest of
the codebase stays portable.

## Inventory

### (a) Trampoline: exe self-rename for `pip install` overwrite

- **Symptom**: `pip install --force-reinstall clud` fails with a permission
  error when any `clud.exe` is already running, because Windows file-locks
  every running executable. The error is surfaced by pip with a generic
  "could not install" message that doesn't make the root cause obvious.

- **Solution**: At the top of `main`, rename `Scripts/clud.exe` to
  `Scripts/clud.exe.old.<rand>` and copy the renamed file back to
  `clud.exe`. The running process continues executing from the
  `.old.<rand>` file (which is now the locked one), while `Scripts/clud.exe`
  becomes a fresh, unlocked copy that `pip` can overwrite. A background
  thread GCs stale `.old.*` files on the next launch. The runtime-cache relay
  (`relay_child_and_wait`) additionally strips `HANDLE_FLAG_INHERIT` from the
  parent's three stdio handles around `CreateProcess` so no detached
  descendant can keep a pipe writer alive past EOF — the class of bug behind
  the 45-minute Windows GHA cancellation investigated in issue #37. The
  daemon detach no longer lives here: since #1186 it goes through
  running-process's daemon spawn, whose `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
  whitelists only the three stdio handles.

- **File**: `crates/clud-bin/src/trampoline.rs` (`unlock_exe`,
  `relay_child_and_wait`); the RAII handle-flag guard
  `windows_stdio::NonInheritableStdioGuard` is in
  `crates/clud-bin/src/platform/windows/trampoline.rs`. The daemon detach is
  `spawn_detached_daemon` in `crates/clud-bin/src/daemon/client.rs`.

- **POSIX behavior**: No-op. Unix lets you `unlink` a running binary; the
  rename dance is unnecessary, so `unlock_exe()` returns immediately on the
  `cfg!(target_os = "windows")` check. The daemon detach uses
  running-process's Unix path: `setsid()` plus a sweep of every fd above 2,
  instead of the Windows `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` flags.

### (b) BatBadBat: `.cmd`/`.bat` rewrite (CVE-2024-24576)

- **Symptom**: `clud --codex` fails with `failed to spawn process: batch
  file arguments are invalid`. Since Rust 1.77 (the CVE-2024-24576 fix),
  `std::process::Command` refuses any batch invocation whose arguments
  don't round-trip losslessly through cmd.exe's parser. npm installs Codex
  as a `.cmd` shim at `%APPDATA%\npm\codex.cmd`, which triggers exactly
  this refusal (issue #59).

- **Solution**: Rewrite the launch as `cmd.exe /D /S /C "<bat-path>" <args>`
  so Rust is spawning cmd.exe (a real `.exe`) and the batch invocation
  lives inside a shell command line where `clud` controls the quoting.
  `running-process-core::CommandSpec::Shell` already provides the outer
  wrapper via `raw_arg` (it builds `cmd /D /S /C "<command>"` verbatim);
  `subprocess.rs` is the *single decision point* that picks `Shell` vs
  `Argv` based on the `argv[0]` extension. Inside the quoted region each
  token is independently wrapped in `"..."` with `%` → `%%` and `"` → `""`
  escapes; everything else (`&`, `|`, `<`, `>`, `^`, `;`) stays literal
  thanks to the outer quotes. The `/D` flag suppresses any user-installed
  `AutoRun` registry key; `/S` makes cmd's quote handling predictable
  (outermost `"..."` stripped verbatim); `/C` runs and exits.
  `subprocess::argv_is_batch_wrapped` exposes the same `.cmd` / `.bat`
  decision to the Ctrl+C teardown path so clud can treat the intermediate
  `cmd.exe` differently from a native backend executable.

- **File**: `crates/clud-bin/src/subprocess.rs`
  (`command_spec_for_subprocess`, `argv_is_batch_wrapped` for Ctrl+C
  teardown gating) over `crates/clud-bin/src/platform/windows/subprocess.rs`:
  the case-insensitive `.cmd`/`.bat` check (`is_batch_wrapper`) and per-arg
  quoting (`quote_for_cmd`).

- **POSIX behavior**: No-op. `is_batch_wrapper` lives only in the Windows
  platform tree; on POSIX every argv stays as `CommandSpec::Argv` and a
  file literally named `codex.cmd` is just treated as an executable (tests
  in `platform/unix/subprocess.rs`).

### (c) `ENABLE_VIRTUAL_TERMINAL_INPUT` RAII

- **Symptom**: Inside `clud --codex`, Backspace doesn't delete anything in
  the Ink TUI. The cause: without `ENABLE_VIRTUAL_TERMINAL_INPUT` (0x0200)
  on the console input handle, `ReadConsoleW` delivers Backspace as `0x08`,
  but xterm-style TUIs (Ink, Codex) expect `0x7F`. The same bit is also
  required for bracketed-paste and other ANSI input sequences to pass
  through unmangled.

- **Solution**: An RAII guard (`ConsoleVtGuard`) that ORs
  `ENABLE_VIRTUAL_TERMINAL_INPUT` into the console-input mode for the
  lifetime of a PTY session and `SetConsoleMode`-restores the saved
  original on drop. The guard returns early without touching the mode if
  stdin is not a real TTY (piped `cargo test`, CI without a console) —
  it remembers `original_mode: None` so the drop impl skips the restore.

- **Output side (#1345, #1374)**: Without
  `ENABLE_VIRTUAL_TERMINAL_PROCESSING` (0x0004), every escape sequence clud
  writes prints literally on a plain conhost window. That covers colored
  `[clud]` notices, selector frames, the graphics header, and child output
  relayed by the PTY pump or the daemon attach. `fn main` calls
  `console_setup::enable_console_vt_output()` before it parses arguments. That
  call ORs the bit into the stdout and stderr console modes, skips a stream
  that is not a console, and never restores it
  ([DD-105](../DESIGN_DECISIONS.md#dd-105-vt-output-processing-is-enabled-once-at-startup-and-never-restored)).
  `selector::run` re-asserts it through the same call. The session's
  `ConsoleVtGuard` also ORs the bit into stdout and restores the prior mode
  on drop.
  Do not call crossterm's `supports_ansi` for this. Its enable is latched by a
  `Once` and never undone. The selector used to call it, so a launch that
  showed a picker rendered escapes and an ordinary repeat launch did not.
  That hid the missing startup enable (#1374). A guard test in
  `console_setup.rs` fails if `fn main` drops the call or a selector module
  calls crossterm's enable again.

- **Deferred wrap (#1942)**: For the session, the guard also ORs
  `DISABLE_NEWLINE_AUTO_RETURN` (0x0008) into the stdout mode
  (`PTY_SESSION_OUTPUT_MODE_BITS`). Without it, conhost wraps eagerly. A
  character in the last column moves the cursor to the next line, and on
  the bottom row it scrolls the screen. A relayed fullscreen TUI such as
  Claude Code's alternate-screen UI draws full-width rows and assumes VT's
  deferred wrap. Each eager wrap shifted Windows Terminal's screen by a line
  the child never repaired, which left fragments of neighbouring lines in
  the left columns. The bit stays session-scoped because it also stops LF
  from implying CR, and clud's own non-PTY output relies on that. A console
  that rejects the bit still gets VT processing. Regression tests:
  `tests/integration/pty/platform/windows/console_wrap.rs` (native) and
  `tests/integration/pty/screen_fidelity.rs` (relay fidelity across a
  resize).

- **Text-cell toasts (#2028)**: The deferred-wrap fix did not end the
  garbling. On Windows Terminal the toast tier was `TextCells`, and the CPU
  banner keeps a toast up for a whole high-CPU episode. A burst that began
  mid-sequence then repainted the entire alternate screen from the
  compositor's `vt100` shadow. Behind ConPTY that shadow cannot match the
  terminal cell for cell, so the repaint wrote shifted text over Claude
  Code's UI. Behind ConPTY the default tier is now the title/status
  fallback (`toast::tier::decide`, `conpty_relay`); `CLUD_TOAST_TIER=text`
  opts back in. The shadow also models the CSI sequences `vt100` drops
  (`toast/shadow_input.rs`).

- **Symptom: output garbles until the window is resized.** A resize makes
  the child repaint its whole screen, so anything that only a resize clears
  means clud's relayed bytes and the terminal's grid disagree. Check, in
  order: the session's stdout mode lacks `DISABLE_NEWLINE_AUTO_RETURN`
  (eager wrap, #1942); a text-cell toast repaint behind ConPTY (only with
  `CLUD_TOAST_TIER=text`, #2028); a launch notice written with per-line
  `eprintln!` while the child's UI is up (#515, #1346). The CPU HUD is a
  kitty image only (`Compositor::cpu_cells` returns nothing outside the
  Kitty tier), and the Claude status-line mirror (#2367, #2395) writes no
  grid bytes, so neither can cause this. `clud --verbose` logs the toast
  decision (`[clud] toasts: tier=… fallback=…`, `runner_execution.rs`);
  ask for that line first when triaging a report.

- **File**: `crates/clud-bin/src/console_setup.rs` holds the mode bits
  (`PTY_SESSION_OUTPUT_MODE_BITS`) and `enable_console_vt_output` over the
  testable `enable_vt_processing`. The Windows half is
  `crates/clud-bin/src/platform/windows/console_setup.rs`: `ConsoleVtGuard`,
  its constructor `enable_console_vt_input`, and the guard's
  `Get/SetConsoleMode` calls (`or_console_mode`, `restore_console_mode`).
  Callers: `runner_execution.rs` (PTY session) and `daemon/attach.rs`.

- **POSIX behavior**: No-op. `platform/unix/console_setup.rs` defines an
  empty `ConsoleVtGuard` with no saved modes and no `Drop` work; its
  `enable_console_vt_input` returns that empty struct, and
  `enable_console_vt_output` sees no console stream. POSIX terminals are
  already in canonical VT mode and need no opt-in.

### (d) Native terminal input via running-process (issues #141 / #575 / #1351)

- **Symptoms**:
  - Conhost strips modifier state from the byte stream, so a byte reader
    cannot distinguish Shift+Enter from plain Enter (#141).
  - Clud's former local translator only handled Enter, Ctrl+V, and nonzero
    Unicode characters. Navigation key records have `UnicodeChar == 0`, so
    arrows and Home/End/Insert/Delete/Page keys were dropped or surfaced as
    malformed CSI suffixes in Codex (#575).
  - Emoji and other characters above U+FFFF arrive as two key records, one per
    UTF-16 surrogate. running-process 4.9 translates each record alone,
    rejects both halves as invalid `char`s, and drops the character: the emoji
    picker, IME commits, and keystroke paste lost them silently (#1351).

- **Solution**: `running_process::pty::terminal_input::TerminalInputCore`
  holds the event queue and owns console-mode selection, generic virtual-key
  translation (`translate_console_key_event`), modifiers, repeat counts, and
  `RUNNING_PROCESS_NATIVE_TERMINAL_INPUT_TRACE_PATH`. Clud runs the
  `ReadConsoleInputW` loop itself (`start_native_reader`, a copy of
  `TerminalInputCore::start_impl` and its worker) so every batch first passes
  through `console_surrogates::SurrogatePairer`: it joins the two halves into
  UTF-8 (across batch boundaries too), ignores key-up records, honors
  `wRepeatCount`, and turns an unpaired half into U+FFFD. Every other record
  goes to the upstream translator unchanged. The upstream fix is
  zackees/running-process#1215; once clud depends on a release with it, the
  loop can go back to `start_impl`. The Windows unit test
  `upstream_translator_alone_drops_both_surrogate_halves` fails when that
  happens. Clud forwards each `TerminalInputEventRecord::data` value as one
  PTY channel chunk, preserving complete sequences such as `ESC [ D`. Its
  adapter changes only two product-specific policies:
  - Shift+Enter's upstream CSI-u representation becomes ESC CR (the Alt+Enter
    newline Claude Code and Codex accept), because ConPTY rewrites a bare LF
    into CR (#1369).
  - Ctrl+V may become a saved clipboard-image path; otherwise the upstream
    control byte passes through.

- **File**: `crates/clud-bin/src/platform/windows/console_input.rs`
  (`spawn_console_input_reader`, `start_native_reader`,
  `translate_key_records`, `adapt_event_with_clipboard`) and the
  platform-neutral `crates/clud-bin/src/console_surrogates.rs`; construction
  and lifetime ownership in `crates/clud-bin/src/runner_execution.rs` (local
  PTY) and `crates/clud-bin/src/daemon/attach_input.rs` (daemon attach).

- **POSIX behavior**: Different mechanism. POSIX terminals deliver
  Shift+Enter as the same `\r` as plain Enter at the kernel layer —
  disambiguation is the terminal emulator's job (for example iTerm's
  "Send literal newline for Shift+Enter") and is out of `clud`'s scope.

### (e) Console title OSC keeper

- **Symptom**: In cmd.exe / Windows Terminal, the title bar otherwise reads
  `Command Prompt` or the path to cmd.exe. Worse, the backend
  (`claude.exe` / `codex.exe`) and any tool it invokes (`git`, `npm`) emit
  OSC 0/2 title-set escape sequences continuously, so even if `clud`
  stamps the title once at launch, the child immediately overwrites it.

- **Solution**: Two complementary defenses.
  1. `set_for_current_cwd()` calls `SetConsoleTitleW` once at launch with
     `clud <cwd-basename>` and records the desired title in a process-wide
     `OnceLock<Arc<Mutex<String>>>` cell.
  2. A daemon thread (`clud-title-keeper`) polls every ~750 ms, backing off
     to 3 s once nothing has changed for four passes (#547); whenever
     `GetConsoleTitleW` reports drift from the desired value, it
     re-stamps via `SetConsoleTitleW`. `OnceLock` guarantees at most one
     keeper thread per process even if `keep_setting_in_background` is
     called multiple times.

  **The keeper is not started at all without a console** (`#706`). `main`
  calls `keep_setting_in_background` *before* subcommand dispatch, so the
  daemon and every worker used to get one too — and there the backoff could
  never engage: `GetConsoleTitleW` returns 0 with no console, so
  `read_console_title()` is `None`, the `current != want` comparison is
  always true, `changed` is pinned true, and `KeeperCadence` resets to
  750 ms on every pass. The result was a permanent 750 ms wake loop — a
  `SetConsoleTitleW` into the void plus a `stat` of the metrics snapshot —
  in exactly the processes that are supposed to be idlest.
  `spawn_keeper_thread` now gates on `GetConsoleCP() != 0` and returns without
  spawning. The cadence state machine is unchanged; it was correct, it simply
  cannot rescue a keeper whose `changed` flag is pinned, which
  `a_keeper_that_always_reports_change_never_backs_off` asserts directly.

  The check is deliberately **not** `GetConsoleWindow`: that returns the
  console's *window* handle and is documented to be null for a console with no
  window, which includes a pseudoconsole. A ConPTY client — clud's own `--pty`
  mode, and Windows Terminal — has a real console and can set its title, so
  gating on a window handle would disable the keeper in exactly the
  environment it exists for. `GetConsoleCP` answers "is a console attached"
  directly: 65001 with one, 0 / `ERROR_INVALID_HANDLE` after `FreeConsole`.

  For PTY-mode launches (`--pty` / POSIX `clud loop`) and `clud attach` the
  `OscTitleStripper` stream filter in the same file eats OSC 0/2 sequences
  from the child's output before they reach the terminal, so the keeper rarely fires.
  Numeric OSC bodies other than `0`/`2` (8 hyperlinks, 10/11 color queries,
  52 clipboard, 133 prompt marks, etc.) pass through verbatim — stripping
  them would break TUIs that rely on the response.

- **File**: `crates/clud-bin/src/console_title.rs`
  (`set_for_current_cwd`, keeper `keep_setting_in_background`); the
  Windows keeper thread and `set_title` in
  `crates/clud-bin/src/platform/windows/console_title.rs`
  (`spawn_keeper_thread`); `OscTitleStripper` in `console_title_osc.rs`.

- **POSIX behavior**: No-op for the keeper half. `spawn_keeper_thread` and
  `set_title` in `platform/unix/console_title.rs` are empty stubs. The `OscTitleStripper` is platform-agnostic because the
  PTY pump runs on every OS.

### (f) `IDropTarget` adapter for terminal drag-drop

- **Symptom**: Dragging a file onto a console window running `clud` on
  Windows produces the OS "no-drop" cursor; conhost rejects the drop at
  the OLE layer (`IDropTarget::DragEnter` → `DROPEFFECT_NONE`) so no bytes
  ever reach `clud`'s stdin (issue #65). Even when registration succeeds,
  Claude Code's backend later registers its own `IDropTarget` and
  displaces ours (issue #79).

- **Solution**: Spawn an STA (Single-Threaded-Apartment) worker thread
  that calls `OleInitialize` and `RegisterDragDrop` on
  `GetConsoleWindow()` — and on the top-level `WindowsTerminal.exe`
  window when present (under Windows Terminal `GetConsoleWindow()` returns
  a `PseudoConsoleWindow` and Explorer hovers over the terminal window
  instead). The thread waits a default 2 s initial delay so Claude Code
  registers first, then re-calls `RegisterDragDrop` every 3 s to displace
  any later re-registration. The IDropTarget callback parses the
  `CF_HDROP` payload via the panic-free `parse_dropfiles_buffer`,
  normalizes each path via `dnd::normalize_dropped_path`, and hands the
  list to a per-launch-mode `DropInjector`. The target reports
  `DROPEFFECT_COPY` only while the payload offers `CF_HDROP` and the
  source allows a copy, and `DROPEFFECT_NONE` otherwise (a text drag,
  a malformed payload, a move-only source). It never claims `MOVE`,
  which would make Explorer delete the dragged file (#1362). The
  injectors:
  - **Subprocess mode**: synthesizes Win32 `INPUT_RECORD` bytes (20-byte
    records, key-down + key-up per char, `VK_RETURN` for `\n`) into the
    console input buffer via `WriteConsoleInputW`.
  - **PTY mode**: writes the joined bytes (`\n`-separated paths plus a
    trailing space) into the PTY master so the slave's TTY reader sees
    them as if typed.

  The RAII guard (`ConsoleDropTargetGuard`) signals the worker, revokes
  each registered window, and calls `OleUninitialize` on the same STA
  thread when dropped — COM lifecycle has to stay on the thread that
  initialized it.

  Which host window gets the extra registration is decided by the pure
  `dnd::drop_host::resolve_drop_host`, from the environment and the
  process ancestor chain (#1358):
  - **Windows Terminal** (`WT_SESSION` set and `WindowsTerminal.exe` the
    nearest recognised host ancestor): register on its visible top-level
    windows as well.
  - **VS Code, its forks and WezTerm** (`TERM_PROGRAM=vscode`/`WezTerm`,
    `WEZTERM_PANE`, or a `Code.exe` / `Code - Insiders.exe` /
    `wezterm-gui.exe` ancestor nearer than any Windows Terminal):
    `GetConsoleWindow()` only. These hosts accept Explorer drops on the
    terminal themselves and type the shell-escaped path into the PTY, and
    their top-level window also holds the editor and every other panel,
    so an `IDropTarget` there (re-taken every 3 s) would capture drops
    meant for the whole IDE. The nearest-host check matters because
    `WT_SESSION` is inherited: a VS Code started from a Windows Terminal
    tab passes it to its own terminals, and the old `WT_SESSION`-only
    gate then registered on the outer Windows Terminal window.
  - **Anything else** (legacy conhost, unknown hosts):
    `GetConsoleWindow()` only; no process snapshot is taken.

- **File**: `crates/clud-bin/src/dnd/console_drop_target.rs`
  (`register_console_drop_target`, `ConsoleDropTargetGuard`, the
  platform-agnostic `dispatch_dropfiles_to_injector` and the effect
  decision `drag_effect`); COM wiring in
  `crates/clud-bin/src/platform/windows/dnd/console_drop_target.rs` and the
  POSIX stub in `platform/unix/dnd.rs`. Injectors in
  `crates/clud-bin/src/dnd/injectors.rs` (`build_input_records`,
  `pty_master_injector`) and `platform/windows/dnd/injectors.rs`
  (`subprocess_console_injector`).

- **POSIX behavior**: Different mechanism. POSIX terminals deliver drops as
  stdin bytes (cmd-style quoted paths, mintty `/c/...` MSYS paths,
  PowerShell `& 'C:\...'`, macOS backslash-escaped spaces, GNOME
  `file://` URIs); the cross-platform `dnd::normalize_dropped_path` and
  `looks_like_dropped_path` string transforms in `dnd/mod.rs`
  handle those. The non-Windows stub of `register_console_drop_target`
  (`platform/unix/dnd.rs`) returns `Err(RegisterError::UnsupportedPlatform)` so POSIX
  call sites simply no-op.

- **Tests**: the COM layer (the `IDropTarget` vtable and the `CF_HDROP`
  `HGLOBAL` extraction) runs on the Windows unit lane against a fake
  `IDataObject`; the `RegisterDragDrop` round trip stays manual. See the
  [dnd README](../../crates/clud-bin/src/dnd/README.md#testing).

### (g) `CREATE_NO_WINDOW` for invisible helper spawns

- **Symptom**: When `clud` spawns daemon-helper / worker / repeat-job
  subprocesses with fully piped stdio on Windows, the OS allocates a
  brand-new conhost window for each child — each allocation is a visible
  flash that steals focus from the developer's window during the
  integration test suite, and from the user's window in production for
  `clud --detach` (issue #55).

- **Solution**: A single source-of-truth helper
  `invisible_helper_creationflags()` that returns
  `Some(CREATE_NO_WINDOW)` (`0x0800_0000`) on Windows and `None`
  elsewhere — exactly the shape `running_process_core::ProcessConfig::creationflags`
  expects. Daemon-side spawn sites OR this into their flags; the
  user-facing backend spawn intentionally does *not* — the user wants
  to see that child's output, and the inherited console means no new
  window is created anyway. A separate helper
  `user_facing_backend_creationflags()` returns
  `Some(CREATE_NEW_PROCESS_GROUP)` (`0x0000_0200`) for the interactive
  backend so the OS skips the child (and its descendants) when delivering
  console `CTRL_C_EVENT`. That keeps confusing Python tracebacks from the
  `nodejs-wheel` distribution out of clud's clean Ctrl+C output — clud
  is then responsible for tearing the child tree down, which it does via
  quirk (i).

- **File**: `crates/clud-bin/src/win_creation_flags.rs`
  (`invisible_helper_flags`, `invisible_helper_creationflags`,
  `new_process_group_flags`, `user_facing_backend_creationflags`, and the
  `CREATE_NO_WINDOW` / `CREATE_NEW_PROCESS_GROUP` constants).

- **POSIX behavior**: All four helpers return `0` / `None` on non-Windows.
  POSIX has no equivalent of `CREATE_NO_WINDOW` — there is no separate
  console window to suppress — and the foreground-process-group /
  `SIGINT` semantics already match what Windows is opting into with
  `CREATE_NEW_PROCESS_GROUP`. Returning `None` (rather than `Some(0)`)
  lets `running-process-core`'s "no override" short-circuit stay intact.

### (h) `process_tree::kill_tree` for Ctrl+C backend-tree reap

- **Symptom**: User hits Ctrl+C in a subprocess-mode backend session and
  the prompt takes several seconds to come back; in the meantime an
  orphaned `node.exe` (the real Claude/Codex backend) keeps writing
  garbage to the inherited console. The
  cause is structural: because of quirk (b) the real process tree at
  runtime is `clud.exe → cmd.exe → node.exe`, and `process.kill()` on
  the direct child reaps only the cmd.exe — the node.exe survives until
  clud itself exits and its Job Object closes.

- **Solution**: Walk the descendant tree with `sysinfo` (using
  `ProcessRefreshKind::nothing()` so the snapshot stays sub-second on
  Windows, where `System::new_all()` takes tens of seconds), then
  `kill_with(Signal::Kill)` + `process.kill()` every descendant
  deepest-first, ending with the root. The cooperative companion
  `try_break_group` calls
  `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)` so a well-behaved
  native agent with a `SetConsoleCtrlHandler` for `CTRL_BREAK_EVENT` can
  flush state during the short grace window before the hard kill follows.
  That cooperative break is skipped when the direct child is the BatBadBat
  `cmd.exe` wrapper from quirk (b): cmd's batch interpreter responds by
  printing `Terminate batch job (Y/N)?` and waiting on stdin. The hard
  `kill_tree` step still runs for both Claude and Codex, so the wrapper and
  backend descendants are reaped without prompting.
  The walk skips any "child" that started before its recorded parent:
  Windows keeps a dead parent's PID in the orphan and recycles PIDs, so a
  raw parent map can walk into an unrelated older tree (#1738; see
  [process-reaping.md](process-reaping.md#parent-links-go-stale-too)).

- **File**: `crates/clud-bin/src/process_tree.rs` (`kill_tree`,
  `descendant_pids`, `should_cooperative_break`, `try_break_group`); the
  native halves in `platform/windows/process_tree.rs` and
  `platform/unix/process_tree.rs`.

- **POSIX behavior**: Same `kill_tree` code path runs (`sysinfo` is
  cross-platform; `Signal::Kill` is SIGKILL on Unix; `process.kill()`
  is a redundant follow-up there but a no-op). `try_break_group` is a
  no-op stub in `platform/unix/process_tree.rs` — POSIX has no
  `CREATE_NEW_PROCESS_GROUP` concept and the terminal already delivers
  SIGINT to clud's foreground process group directly. The Unix test there
  (`kill_tree_terminates_real_descendant_on_unix`) spawns
  `sh -c 'sleep 30'` to mirror the `clud → cmd → child` shape and
  asserts the parent is reaped within 5 s.

### (i) Codex hook batch wrappers need `$LASTEXITCODE`

- **Symptom**: `clud --codex` prints:

  ```text
  [clud] warning: Codex hook command in ...\.codex\hooks.json uses a Windows batch wrapper without explicit `$LASTEXITCODE` propagation; a blocking hook may fail open.
  ```

  This is not a backend launch failure. It is a hook-health warning for Codex
  `PreToolUse` commands that mention `.cmd` or `.bat` without also mentioning
  `$LASTEXITCODE`. On Windows, many npm-installed tools are batch wrappers,
  and a PowerShell hook command that does not explicitly exit with the native
  command's last exit code can report success to Codex after the wrapped hook
  failed. For a blocking hook, that is a fail-open permission path.

- **Solution**: `hook_health::warn_on_powershell_exit_code_risk` scans Codex
  hook command strings during the `--codex` launch parity check. The diagnostic
  is warning-only; clud never edits `hooks.json` for this case because the safe
  repair depends on the user's hook command. The user should either call a
  native executable directly or make the PowerShell command end with
  `exit $LASTEXITCODE` after invoking the batch wrapper.

- **File**: `crates/clud-bin/src/hook_health/inspect.rs`
  (`warn_on_powershell_exit_code_risk`); launch gating at
  `hook_health/mod.rs` (`should_check_launch`). The unit coverage for hook
  parity lives in `crates/clud-bin/src/hook_health_tests.rs`.

- **POSIX behavior**: No-op. The scanner returns immediately unless
  `cfg!(target_os = "windows")` is true.

### (j) Claude Code hook stdin EOF/TTY bug cluster

- **Symptom**: Claude Code hooks on Windows can hang until their hook timeout
  when the hook script does an unbounded read such as
  `sys.stdin.read()`, `json.load(sys.stdin)`, or Node's
  `process.stdin.on("end", ...)`. The upstream issue cluster reports three
  related shapes: stdin is left open without EOF, stdin arrives empty, or
  stdin is attached as a TTY instead of a pipe. The visible difference from a
  clud policy denial is important: a policy denial returns immediately with
  deny output / exit code 2; a stdin bug timeout often shows the hook blocked
  inside the read call. See
  <https://github.com/anthropics/claude-code/issues/53177> and duplicate/root
  reports <https://github.com/anthropics/claude-code/issues/46177>,
  <https://github.com/anthropics/claude-code/issues/48009>, and
  <https://github.com/anthropics/claude-code/issues/36156>.

- **Solution**: `hook_health::collect_claude` emits a Windows-only warning
  whenever Claude Code hook settings are present. The diagnostic points to the
  upstream bug cluster and preserves the reported workaround: set
  `CLAUDE_CODE_GIT_BASH_PATH` to Git for Windows' real `bin\bash.exe`, not
  `git-bash.exe`. A typical `~/.claude/settings.json` entry is:

  ```json
  {
    "env": {
      "CLAUDE_CODE_GIT_BASH_PATH": "C:\\Program Files\\Git\\bin\\bash.exe"
    }
  }
  ```

  Run `where bash` to locate candidates, but check the result: the desired
  path ends in `Git\bin\bash.exe`. Avoid `git-bash.exe` because that is the
  MinTTY launcher, not the non-MinTTY Bash executable Claude Code needs for
  this workaround. clud's own managed hooks also avoid unbounded stdin reads:
  the native `clud-block-bad-cmd` hook binary uses bounded pipe reads, and
  `telemetry.py` uses the same timeout-safe pattern so an open hook stdin pipe
  cannot wedge the hook. The managed `block-bad-cmd.py` file is only a
  compatibility shim that execs the native binary.

- **File**: `crates/clud-bin/src/hook_health/inspect.rs`
  (`warn_on_claude_windows_stdin_bug`);
  `crates/clud-bin/src/block_bad_cmd.rs`
  (`read_stdin_bounded`);
  `crates/clud-bin/assets/tools/hooks/telemetry.py`
  (`_read_stdin_bounded`). Runtime coverage lives in
  `tests/test_hook_stdin.py`; the hook-health diagnostic guardrail lives in
  `crates/clud-bin/src/hook_health_tests.rs`.

- **POSIX behavior**: No-op. The diagnostic returns immediately unless
  `cfg!(target_os = "windows")` is true, and POSIX hook subprocesses receive
  normal pipe EOF semantics from Claude Code.

### (k) Foreground tool-shell lifecycle tracking

- **Symptom**: The original #569 foreground Job listener treated every
  `cmd.exe`, PowerShell, or Bash process at every depth as a tool shell. When a
  nested wrapper exited (`PowerShell -> new.exe -> cmd /c start cmd`), its
  subtree was killed even though the final terminal was intentionally
  detached. The same basename rule could target `conhost.exe` below a kill
  root; killing a console host destroys the console while its client can remain
  alive but headless (#612, #616).

- **Role model**: After `main` ensures the persistent daemon, the foreground
  CLI assigns itself to an otherwise empty Job Object with an I/O completion
  port and no `KILL_ON_JOB_CLOSE` limit. `runner.rs` registers the exact spawned
  backend root with
  `ForegroundJobTracker` after `NativeProcess` / `NativePtyProcess` starts.
  Registration stores PID **and process start time**, so PID reuse never
  grants stale backend authority. The pure planner in
  `job_orphan_reaper.rs` walks captured metadata in phases:
  `backend root -> bootstrap -> exact agent host` (`codex.exe`, native
  `claude.exe`, or the npm Claude launcher's first `node.exe`).
  Only a shell that is a **direct child of that exact agent host** is a tool
  root. Once the walk crosses the agent boundary into any non-shell client,
  every descendant remains a client even if its image is `node.exe`,
  `python.exe`, or another shell.

- **Completion decisions**:
  - `bash.exe -> bash.exe` is a recognized Git-for-Windows re-exec handoff.
    The inner shell inherits completion ownership; the outer exit does not reap.
  - A live client below the completed tool root is reaped.
  - A nested shell reached through a non-shell client is a detach boundary and
    its subtree is spared (`!new`).
  - `RUNNING_PROCESS_IS_DAEMON` is the positive detach contract for services
    and helpers. The daemon PID and its whole subtree are spared. Ordinary
    unmarked Docker helpers hosted below `conhost.exe` or sidecar
    `OpenConsole.exe` are also protected by
    the unconditional console-host boundary.
  - `conhost.exe` and sidecar `OpenConsole.exe` are never automatic kill targets.
    The runtime takes a fresh
    process snapshot immediately before each kill and prunes every console-host
    subtree, even if its Job NEW_PROCESS event raced metadata publication.
  - A Job `NEW_PROCESS` PID whose Toolhelp metadata is not published yet stays
    unresolved and is retried during completion-port quiet periods. Empty
    shells stabilize for one quiet period before finalization. If a process
    exits before any metadata observation succeeds, cleanup fails closed,
    records a structured metadata-miss event, and never grants kill authority
    from the bare PID.

- **Diagnostics**: Every actual tool-root completion writes one or more
  `foreground_tool_shell_decision` JSONL events to
  `~/.clud/state/daemon-events.jsonl`. Fields include foreground PID, trigger
  shell PID/image/role, candidate root PID/image, `action`
  (`reap`/`spare`/`handoff`), candidate start time, and a machine-readable
  reason.

- **Coverage**: Platform-neutral fixtures hard-gate leaked git-client reap,
  Git Bash handoff, `!new` survival, declared-daemon survival, conhost survival,
  exact agent-boundary behavior, and stale-PID rejection. The Windows
  `tool_shell_lifecycle_windows` integration test exercises the real Job
  completion port with one should-reap and one must-survive tree.

- **POSIX behavior**: Unchanged. `ForegroundJobTracker::install()` returns
  `None`, and backend registration is a no-op.

## Cross-cutting patterns

- **`#[cfg(windows)]` placement is module-local.** Each quirk lives in its
  own module; `cfg`-gating happens at the function, field, or impl
  boundary so call sites can call the public API unconditionally.
  `ConsoleVtGuard` returns the same type on every OS (the field is gated);
  `invisible_helper_creationflags` returns `Option<u32>` on every OS (the
  value, not the signature, is gated); `try_break_group` returns `bool`
  on every OS (the body is gated). The rest of the codebase never wraps
  its own call sites in a `cfg!` check.

- **RAII for console state.** Anything that mutates global console state
  returns a guard whose `Drop` impl restores the previous state:
  - `console_setup::ConsoleVtGuard` (`platform/windows/console_setup.rs`) restores the
    saved console-input mode.
  - `dnd::console_drop_target::ConsoleDropTargetGuard`
    (`dnd/console_drop_target.rs`) revokes each registered window
    and calls `OleUninitialize` on the same STA thread.
  - `windows_stdio::NonInheritableStdioGuard`
    (`platform/windows/trampoline.rs`) restores `HANDLE_FLAG_INHERIT` on the three
    stdio handles after the runtime-cache relay's spawn returns.

- **Single decision point for the `.cmd` rewrite.** `subprocess.rs` is the
  *only* file that knows about BatBadBat. Every backend spawn goes
  through `command_spec_for_subprocess` and gets the right `CommandSpec`
  variant back — nothing else has a special case for `.cmd` / `.bat`.

- **Single source of truth for `CREATE_NO_WINDOW`.** Every daemon-helper
  spawn imports from `win_creation_flags` rather than defining the
  `0x0800_0000` literal locally. The literal is anchored by
  `create_no_window_value_matches_winapi` in
  `platform/windows/win_creation_flags_tests.rs` (Windows) and
  `invisible_helper_flags_is_zero_off_windows` in
  `platform/unix/win_creation_flags_tests.rs` (POSIX) so a typo in either branch fails CI.

- **Best-effort, non-fatal startup.** The trampoline, the title keeper,
  the IDropTarget worker, and the `.old.*` GC are all wrapped so a
  failure logs to stderr (or stays silent) and the launch continues.
  None of them can block a `clud` invocation.

## Testing coverage

Most of these modules are no-op stubs on POSIX, which means the Linux/macOS
unit-test runs cover only the dispatch logic, not the OS calls themselves.

- On the Windows matrix, `console_input` unit tests construct upstream
  `TerminalInputEventRecord`s and pin clud's Shift+Enter/Ctrl+V policy plus
  atomic event forwarding, and pass emoji `KEY_EVENT_RECORD`s through
  `translate_key_records` and the real upstream translator (#1351). The
  surrogate-pairing rules themselves live in `console_surrogates.rs` and are
  unit-tested on every OS. The Windows integration test
  `tests/integration/pty/platform/windows/shift_enter_dual_reader.rs` passes native `KEY_EVENT_RECORD`s through
  running-process's real translator and verifies arrow/Home/End/Insert/Delete/
  Page sequences plus trace bytes even on a headless runner. When stdin is an
  attached console, the same test additionally injects records (including an
  emoji's two surrogate records) with `WriteConsoleInputW` and observes the
  production reader.

- Cross-platform, `console_title::OscTitleStripper` is a pure byte filter and is
  fully unit-tested cross-platform — including split-across-chunks,
  back-to-back OSCs, and passthrough for OSC 8/10/52/133. See the test
  cases in `crates/clud-bin/src/console_title_osc.rs`.

The other quirks (`trampoline::unlock_exe`,
`subprocess::command_spec_for_subprocess` Windows branch,
`console_setup::enable_console_vt_input`,
`dnd::console_drop_target::register_console_drop_target`,
`process_tree::kill_tree` Windows tree shape) have `#[cfg(windows)]` tests
that only run on the Windows x86 and ARM matrix jobs. The cross-platform
side of `process_tree::kill_tree` is covered by the Unix-only test in
`platform/unix/process_tree.rs` so the descendant-walk contract is enforced on every
host.

## See also

- [session-lifecycle.md](session-lifecycle.md) — how `console_setup`,
  `console_title`, and `console_input` feed into the PTY pump and the
  interactive-hooks loop.
- [../../crates/clud-bin/src/dnd/README.md](../../crates/clud-bin/src/dnd/README.md)
  — the drag-and-drop subsystem in detail, including the per-launch-mode
  injector contract and the `CF_HDROP` wire format.
- [../../crates/clud-bin/src/voice/README.md](../../crates/clud-bin/src/voice/README.md)
  — the F3 voice-mode pipeline, including the Windows ARM carve-out for
  transcription.

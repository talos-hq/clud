use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::execute;
use running_process::pty::NativePtyProcess;

use crate::console_title::OscTitleStripper;
use crate::dnd::{looks_like_dropped_path, normalize_dropped_path};
use crate::graphics::GraphicsConfig;
use crate::verbose_log;

#[path = "session_output.rs"]
mod session_output;
#[cfg(test)]
use session_output::run_output_writer;
use session_output::{redraw_graphics_header_for_resize, run_output_writer_composited, OutputMsg};
#[path = "session_stdin.rs"]
pub(crate) mod session_stdin;
#[cfg(test)]
use session_stdin::stdin_chunk_requests_interrupt;
use session_stdin::{
    normalize_interactive_console_stdin_chunk, should_normalize_interactive_console_stdin,
    should_spawn_byte_stream_stdin_reader, stdin_source_is_real_stdin, InterruptScanner,
};

/// Resize the PTY. On Windows, `running_process::pty::NativePtyProcess::resize_impl`
/// is a deliberate no-op (see that crate's `pty/mod.rs:730-737`), so reaching
/// the underlying master's `resize()` directly is the only way to honor a
/// `SIGWINCH`/`Event::Resize`. On POSIX the library's implementation does the
/// right thing, so delegate. Issue #31, theory T2.
pub fn resize_pty(process: &NativePtyProcess, rows: u16, cols: u16) -> io::Result<()> {
    crate::platform::session::resize_pty(process, rows, cols)
}

/// Counts of F3 events observed in a stream chunk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct F3Events {
    /// Number of F3 press events seen. Repeats (autorepeat) are intentionally
    /// not counted as new presses — they indicate the key is still held.
    pub presses: u32,
    /// Number of F3 release events seen. Only fires on terminals that
    /// implement the kitty keyboard protocol with REPORT_EVENT_TYPES.
    pub releases: u32,
}

/// Byte-level observer that reports F3 press / release events seen in a
/// stream, without modifying the bytes. The raw pump forwards every byte
/// to the child verbatim and asks this observer how many F3 events flowed
/// past so it can call `InteractiveHooks::on_f3_press` / `on_f3_release`
/// once per event.
///
/// Three encodings are matched, covering the cross-platform terminal
/// matrix:
///
/// * Legacy SS3 form `\x1bOR` — emitted by Windows ConPTY and most POSIX
///   terminals without kitty keyboard protocol. Press-only.
/// * CSI tilde form `\x1b[13~` — emitted by xterm and most Linux consoles.
///   Press-only by default; with kitty REPORT_EVENT_TYPES enabled the
///   terminal extends it to `\x1b[13;1:3~` for release, `\x1b[13;1:2~`
///   for repeat.
/// * Kitty CSI u form `\x1b[13u` or `\x1b[57346u` (functional encoding) —
///   press-only by default; the `;mod:event-type` suffix carries
///   release/repeat the same way.
///
/// Issue #13 hold-to-record relies on the release branch. Terminals that
/// don't emit release events (notably ConPTY) fall back to the
/// VAD-silence auto-stop inside the voice module — see `voice.rs`.
///
/// The state machine survives across `observe` calls, so any of these
/// sequences split across reads (even one byte at a time) still fires
/// exactly once.
pub struct F3Observer {
    state: F3State,
    /// Parameter bytes accumulated between `\x1b[` and a CSI terminator.
    /// Capped at MAX_CSI_LEN to keep a runaway terminal from growing this
    /// unboundedly.
    csi_buf: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
enum F3State {
    Idle,
    Esc,
    /// Saw `\x1bO`; one more byte and we know if this is SS3-R (F3 press).
    Ss3,
    /// Saw `\x1b[`; accumulating parameter bytes until a CSI terminator.
    Csi,
}

/// Max parameter-byte payload a CSI sequence can have before we abandon
/// the match. A real F3 event tops out at ~16 bytes (`\x1b[57346;1:3u`),
/// 64 is generous and bounds memory if the terminal is misbehaving.
const MAX_CSI_LEN: usize = 64;

impl F3Observer {
    pub fn new() -> Self {
        Self {
            state: F3State::Idle,
            csi_buf: Vec::new(),
        }
    }

    /// Scan `chunk` and return the F3 events it contains. Updates internal
    /// state so subsequent calls see continuing matches.
    pub fn observe(&mut self, chunk: &[u8]) -> F3Events {
        let mut events = F3Events::default();
        for &b in chunk {
            match self.state {
                F3State::Idle => {
                    if b == 0x1b {
                        self.state = F3State::Esc;
                    }
                }
                F3State::Esc => match b {
                    b'O' => self.state = F3State::Ss3,
                    b'[' => {
                        self.state = F3State::Csi;
                        self.csi_buf.clear();
                    }
                    0x1b => {} // stay in Esc, a new sequence is starting
                    _ => self.state = F3State::Idle,
                },
                F3State::Ss3 => match b {
                    b'R' => {
                        // \x1bOR — F3 press in SS3 encoding.
                        events.presses += 1;
                        self.state = F3State::Idle;
                    }
                    0x1b => self.state = F3State::Esc,
                    _ => self.state = F3State::Idle,
                },
                F3State::Csi => {
                    if is_csi_terminator(b) {
                        if let Some(kind) = parse_f3_csi(&self.csi_buf, b) {
                            match kind {
                                F3Kind::Press => events.presses += 1,
                                F3Kind::Release => events.releases += 1,
                                // Repeat = key still held; deliberately silent.
                                F3Kind::Repeat => {}
                            }
                        }
                        self.state = F3State::Idle;
                        self.csi_buf.clear();
                    } else if b == 0x1b {
                        // Nested escape — abandon this CSI, start a new sequence.
                        self.state = F3State::Esc;
                        self.csi_buf.clear();
                    } else if self.csi_buf.len() < MAX_CSI_LEN {
                        self.csi_buf.push(b);
                    } else {
                        // Overrun — give up on this sequence.
                        self.state = F3State::Idle;
                        self.csi_buf.clear();
                    }
                }
            }
        }
        events
    }
}

impl Default for F3Observer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum F3Kind {
    Press,
    Repeat,
    Release,
}

/// CSI terminator bytes per ECMA-48 (`0x40..=0x7E`, "Final Byte"). We
/// only care about a couple in practice (`u`, `~`) but accepting the
/// full range keeps misbehaving terminals from getting us stuck inside
/// `F3State::Csi`.
fn is_csi_terminator(b: u8) -> bool {
    matches!(b, 0x40..=0x7E)
}

/// Decide whether a parameter-bytes payload (e.g. `13;1:3`) plus a
/// terminator (e.g. `~` or `u`) is an F3 event, and which kind.
/// Returns `None` for anything that isn't F3 — different keycodes,
/// non-keyboard CSI sequences, malformed payloads.
fn parse_f3_csi(params: &[u8], terminator: u8) -> Option<F3Kind> {
    if terminator != b'u' && terminator != b'~' {
        return None;
    }
    let payload = std::str::from_utf8(params).ok()?;
    let mut parts = payload.split(';');
    let keycode_str = parts.next()?;

    // First param is the keycode. F3 keycodes:
    //   - `\x1b[13~` (CSI tilde, legacy F3 — see Linux/xterm function-key map)
    //   - `\x1b[13u` (CSI u, F3 with disambiguation but no functional encoding)
    //   - `\x1b[57346u` (CSI u, F3 with kitty functional encoding)
    let is_f3 = match terminator {
        b'~' => keycode_str == "13",
        b'u' => keycode_str == "13" || keycode_str == "57346",
        _ => false,
    };
    if !is_f3 {
        return None;
    }

    // Second param is `modifier[:event-type[:text]]`. Event-type defaults
    // to 1 (press) when omitted.
    let event_type = parts
        .next()
        .and_then(|modifier_field| modifier_field.split(':').nth(1))
        .and_then(|et| et.parse::<u32>().ok())
        .unwrap_or(1);

    match event_type {
        2 => Some(F3Kind::Repeat),
        3 => Some(F3Kind::Release),
        _ => Some(F3Kind::Press),
    }
}

/// Where to send synthetic input bytes generated by an `InteractiveHooks`
/// implementation (voice transcript, drag-drop paths, etc.).
///
/// Two impls ship in-tree:
/// * `NativePtyProcessSink` — wraps a `&NativePtyProcess` and forwards
///   to `write_impl`. Used by the direct local-PTY pump in `runner.rs`.
/// * A TCP-backed sink in `daemon::attach` — sends `WorkerClientMessage::Input`
///   frames to the daemon worker. Used by centralized-mode foreground
///   attach so voice + DnD reach the daemon-owned PTY just like keystrokes.
///
/// The `submit` flag mirrors `NativePtyProcess::write_impl`'s second
/// argument: `true` ends the input with a synthetic Enter so the agent
/// processes the buffer, `false` leaves the cursor mid-line so a human
/// can edit before submitting.
pub trait PtyInputSink {
    fn write_input(&mut self, bytes: &[u8], submit: bool) -> io::Result<()>;
}

/// Adapter: a `PtyInputSink` over a local `NativePtyProcess`.
pub struct NativePtyProcessSink<'a> {
    process: &'a NativePtyProcess,
}

impl<'a> NativePtyProcessSink<'a> {
    pub fn new(process: &'a NativePtyProcess) -> Self {
        Self { process }
    }
}

impl<'a> PtyInputSink for NativePtyProcessSink<'a> {
    fn write_input(&mut self, bytes: &[u8], submit: bool) -> io::Result<()> {
        self.process
            .write_impl(bytes, submit)
            .map_err(|err| io::Error::other(err.to_string()))
    }
}

pub trait InteractiveHooks {
    fn intercept_f3(&self) -> bool {
        false
    }

    fn on_f3_press(&mut self, _sink: &mut dyn PtyInputSink) -> io::Result<()> {
        Ok(())
    }

    fn on_f3_release(&mut self, _sink: &mut dyn PtyInputSink) -> io::Result<()> {
        Ok(())
    }

    fn on_tick(&mut self, _sink: &mut dyn PtyInputSink) -> io::Result<()> {
        Ok(())
    }
}

/// Enable raw mode and keyboard-enhancement flags on the current
/// terminal, returning a guard that restores the original state on drop.
/// Only useful when stdin is an actual TTY; see `enter_raw_mode_if_tty`.
#[derive(Debug)]
pub struct RawTerminalGuard {
    keyboard: KeyboardEnhancementGuard,
}

/// Owns clud's own kitty keyboard-enhancement stack frame for one terminal
/// session, plus the tracker for the terminal state the child leaves on:
/// frames it pushes over clud's, and the modes [`child_modes`] follows.
///
/// Every path that relays a child's output to the user's terminal holds one:
/// the local PTY pump through [`RawTerminalGuard`], and the daemon attach
/// (`daemon::attach::attach_to_session`) directly, because its raw-mode guard
/// ends before its output relay does (#1363). Unwinding lives in `Drop`, so
/// a normal exit, an interrupt, an early return and a panic all leave the
/// terminal's pre-session keyboard state as they found it (#1221), and turn
/// off what a killed child left on.
#[derive(Debug)]
pub struct KeyboardEnhancementGuard {
    pushed: bool,
    child: Arc<KeyboardEnhancementTracker>,
}

/// Tracks the terminal state a child writes to the outer terminal: the
/// keyboard-enhancement stack frames it pushes, and the modes it turns on.
/// A child TUI may change both for itself, but Ctrl+C kills it before its
/// own exit path can undo them. Keep this separate from the terminal's
/// pre-existing state: cleanup must undo only what was observed after clud
/// started the child (issues #1221, #1704).
#[derive(Debug, Default)]
pub struct KeyboardEnhancementTracker {
    state: Mutex<KeyboardEnhancementTrackerState>,
    modes: Mutex<child_modes::ChildModes>,
}

#[derive(Debug, Default)]
struct KeyboardEnhancementTrackerState {
    /// Incomplete CSI sequence retained across PTY read boundaries.
    pending: Vec<u8>,
    unbalanced_pushes: usize,
}

impl KeyboardEnhancementTracker {
    /// Observe unmodified child output. Among kitty sequences only the stack
    /// operations (`CSI > ... u` and `CSI < ... u`) count; ordinary CSI-u key
    /// events such as Ctrl+C are deliberately ignored.
    pub fn observe(&self, bytes: &[u8]) {
        let mut state = self.state.lock().expect("keyboard tracker lock");
        for &byte in bytes {
            observe_keyboard_enhancement_byte(&mut state, byte);
        }
        drop(state);
        self.modes.lock().expect("child modes lock").observe(bytes);
    }

    fn take_unbalanced_pushes(&self) -> usize {
        let mut state = self.state.lock().expect("keyboard tracker lock");
        std::mem::take(&mut state.unbalanced_pushes)
    }

    /// The bytes that turn off the modes the child left on; see
    /// [`child_modes`].
    fn take_mode_reset(&self) -> Vec<u8> {
        self.modes.lock().expect("child modes lock").take_reset()
    }

    /// Everything needed to undo what the observed output left on the terminal,
    /// for a mirror that has no [`RawTerminalGuard`] of its own.
    pub(crate) fn take_child_reset(&self) -> Vec<u8> {
        let mut reset = self.take_mode_reset();
        reset.extend(keyboard_enhancement_pop_bytes(
            self.take_unbalanced_pushes(),
        ));
        reset
    }
}

fn observe_keyboard_enhancement_byte(state: &mut KeyboardEnhancementTrackerState, byte: u8) {
    if state.pending.is_empty() {
        if byte == b'\x1b' {
            state.pending.push(byte);
        }
        return;
    }

    state.pending.push(byte);
    match state.pending.len() {
        2 if state.pending.as_slice() != b"\x1b[" => {
            state.pending.clear();
            if byte == b'\x1b' {
                state.pending.push(byte);
            }
        }
        2 => {}
        3 if !matches!(state.pending[2], b'>' | b'<') => {
            state.pending.clear();
            if byte == b'\x1b' {
                state.pending.push(byte);
            }
        }
        3 => {}
        4..=18 => {
            if byte == b'u' {
                let operation = state.pending[2];
                let parameters = &state.pending[3..state.pending.len() - 1];
                if parameters.iter().all(u8::is_ascii_digit) {
                    if operation == b'>' {
                        state.unbalanced_pushes = state.unbalanced_pushes.saturating_add(1);
                    } else {
                        let requested = if parameters.is_empty() {
                            1
                        } else {
                            std::str::from_utf8(parameters)
                                .ok()
                                .and_then(|value| value.parse::<usize>().ok())
                                .unwrap_or(0)
                        };
                        state.unbalanced_pushes = state.unbalanced_pushes.saturating_sub(requested);
                    }
                }
                state.pending.clear();
            } else if !byte.is_ascii_digit() {
                state.pending.clear();
                if byte == b'\x1b' {
                    state.pending.push(byte);
                }
            }
        }
        _ => {
            state.pending.clear();
            if byte == b'\x1b' {
                state.pending.push(byte);
            }
        }
    }
}

/// Turns off every input mode a child TUI may have enabled and shows the
/// cursor again.
///
/// clud never enables any of these itself (see `toast::mouse`), so "off" is
/// the terminal's pre-session state. A child that exits (or is killed)
/// without sending its own disable leaves them on, and the shell then echoes
/// `35;21;8M…` on every mouse move (#1383), `^[[I` / `^[[O` on every focus
/// change, and `^[[200~` around every paste (#1701):
///
/// - mouse: `1000` `1001` `1002` `1003` `1005` `1006` `1015` `1016`
/// - `1004` focus reporting, `2004` bracketed paste
/// - `25` cursor visibility (set, not reset)
const CHILD_TERMINAL_MODES_RESET: &[u8] = b"\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?1004l\x1b[?2004l\x1b[?25h";

fn keyboard_enhancement_pop_bytes(count: usize) -> Vec<u8> {
    // crossterm's PopKeyboardEnhancementFlags emits this one-frame pop. Do
    // not use `CSI = ... u`: that would overwrite the caller's state instead
    // of unwinding only the frames that belong to this session.
    b"\x1b[<1u".repeat(count)
}

/// Public factory for a `RawTerminalGuard` returning `None` when stdin
/// is piped (no point putting a pipe into raw mode). `run_plan_pty` in
/// main.rs owns the guard for the duration of the pump call so the
/// terminal is restored even if the pump panics.
pub fn enter_raw_mode_if_tty() -> Option<RawTerminalGuard> {
    if terminals_are_interactive() {
        RawTerminalGuard::enter().ok()
    } else {
        None
    }
}

/// Keyboard-enhancement flags clud pushes for the lifetime of a PTY session.
///
/// **`DISAMBIGUATE_ESCAPE_CODES` is deliberately absent** (issue #1101).
/// That flag makes a kitty-protocol terminal re-encode every Ctrl chord as a
/// CSI u sequence, so Ctrl+C arrives as `\x1b[99;5u` instead of byte `0x03`.
/// It took away the ability to interrupt a session two ways at once:
/// `stdin_chunk_requests_interrupt` saw no `0x03`, and raw mode had already
/// cleared `ISIG` so no SIGINT reached the `ctrlc` handler either. The bytes
/// were then forwarded verbatim to a child that does not speak the protocol,
/// whose PTY echoed them back as literal text — holding Ctrl+C flooded the
/// terminal with `^[[99;5:2u` and a 200-iteration `clud grind` could only be
/// stopped by closing the window.
///
/// `REPORT_EVENT_TYPES` is kept because it is the flag that asks the terminal
/// for key release at all, which is what issue #13's hold-to-record needs.
/// `F3Observer` already matches the event type on both the tilde and CSI u
/// forms (`parse_f3_csi`), so whichever spelling a terminal uses is counted.
///
/// What is **not** claimed here is that every kitty-protocol terminal attaches
/// event types to the legacy `\x1b[13~` encoding once disambiguate is off. The
/// protocol spec presents the two flags as independent and states no
/// dependency, but does not spell that interaction out. If some terminal turns
/// out to report release only for CSI u forms, F3 hold-to-record there
/// degrades to the VAD-silence auto-stop `voice.rs` already uses as its
/// ConPTY fallback — a precision loss in an optional feature, deliberately
/// traded against being unable to interrupt a session at all.
///
/// Both flags were pushed together in a4ef5f5 (2026-04-14), four weeks before
/// the voice feature that now documents them existed.
const KEYBOARD_ENHANCEMENT_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::REPORT_EVENT_TYPES;

impl KeyboardEnhancementGuard {
    /// Push `KEYBOARD_ENHANCEMENT_FLAGS` onto the real terminal. A console
    /// without VT support rejects the push; the guard then owns no frame and
    /// pops none, but still unwinds frames a child pushes.
    pub fn push() -> Self {
        let pushed = execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KEYBOARD_ENHANCEMENT_FLAGS)
        )
        .is_ok();
        Self::with_pushed(pushed)
    }

    /// A guard that did (`true`) or did not push clud's own frame, without
    /// touching the terminal. For tests that drive the restore directly.
    #[doc(hidden)]
    pub fn with_pushed(pushed: bool) -> Self {
        Self {
            pushed,
            child: Arc::new(KeyboardEnhancementTracker::default()),
        }
    }

    /// Share the child-output tracker with whatever writes the child's
    /// output to the terminal. Feed it the bytes before they are written.
    pub fn child_tracker(&self) -> Arc<KeyboardEnhancementTracker> {
        Arc::clone(&self.child)
    }

    /// Undo what the child left on: turn off its modes and pop its frames,
    /// keeping clud's own frame. Call it only once the child's output relay
    /// has stopped, so everything that reached the terminal was observed.
    pub fn restore_child_state(&self) {
        let mut stdout = io::stdout();
        self.restore_child_state_to(&mut stdout);
        let _ = stdout.flush();
    }

    fn restore_child_state_to(&self, out: &mut dyn Write) {
        let _ = out.write_all(&self.child.take_mode_reset());
        let count = self.child.take_unbalanced_pushes();
        if count != 0 {
            let _ = out.write_all(&keyboard_enhancement_pop_bytes(count));
        }
    }

    /// Undo the child's state, then pop clud's own frame, in LIFO order.
    /// Idempotent: a second call writes nothing.
    pub fn unwind_to(&mut self, out: &mut dyn Write) {
        self.restore_child_state_to(out);
        if std::mem::take(&mut self.pushed) {
            let _ = out.write_all(&keyboard_enhancement_pop_bytes(1));
        }
    }
}

impl Drop for KeyboardEnhancementGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        self.unwind_to(&mut stdout);
        let _ = stdout.flush();
    }
}

impl RawTerminalGuard {
    pub fn enter() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self {
            keyboard: KeyboardEnhancementGuard::push(),
        })
    }

    /// Share the child-output tracker with the PTY reader for this session.
    pub fn child_keyboard_enhancement_tracker(&self) -> Arc<KeyboardEnhancementTracker> {
        self.keyboard.child_tracker()
    }

    /// Undo child-owned modes and frames before this guard pops clud's own
    /// frame. The pump joins its output reader before this is called, so
    /// every child control sequence that reached the terminal was observed.
    pub fn restore_child_terminal_state(&self) {
        self.keyboard.restore_child_state();
    }
}

impl RawTerminalGuard {
    /// Everything the guard writes on the way out, in order: what the child
    /// left on (tracked modes, then its keyboard frames), the blanket reset
    /// of modes that are always off outside a session, then clud's own frame.
    fn write_exit_reset(&mut self, out: &mut dyn Write) {
        self.keyboard.restore_child_state_to(out);
        let _ = out.write_all(CHILD_TERMINAL_MODES_RESET);
        self.keyboard.unwind_to(out);
    }
}

impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        // This is intentionally in Drop so unwind cannot strand a child frame.
        let mut stdout = io::stdout();
        self.write_exit_reset(&mut stdout);
        let _ = stdout.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// True when both stdin and stdout are terminals.
///
/// Under Git Bash's mintty (without winpty) a native Windows exe gets pipe
/// stdio, so this returns false even though a human is typing (#1357).
/// That is correct — pipes cannot do raw mode or ConPTY — but the
/// downgrade is announced by [`warn_if_mintty_without_console`].
pub fn terminals_are_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Pure decision behind [`mintty_without_console`]: Windows, both stdio
/// handles are non-terminals, and the environment looks like an MSYS2 /
/// Git Bash terminal (a real `TERM` plus a non-empty `MSYSTEM`).
pub(crate) fn looks_like_mintty_without_console(
    is_windows: bool,
    stdin_tty: bool,
    stdout_tty: bool,
    term: Option<&str>,
    msystem: Option<&str>,
) -> bool {
    let term_ok = matches!(term, Some(t) if !t.is_empty() && t != "dumb");
    let msystem_ok = matches!(msystem, Some(m) if !m.is_empty());
    is_windows && !stdin_tty && !stdout_tty && term_ok && msystem_ok
}

/// Whether clud appears to be running under mintty without a Windows console.
pub fn mintty_without_console() -> bool {
    let term = std::env::var("TERM").ok();
    let msystem = std::env::var("MSYSTEM").ok();
    looks_like_mintty_without_console(
        crate::platform::host::WINDOWS,
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        term.as_deref(),
        msystem.as_deref(),
    )
}

/// Print a one-time stderr warning when [`mintty_without_console`] holds,
/// unless `CLUD_NO_MINTTY_WARNING` is set.
pub fn warn_if_mintty_without_console() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("CLUD_NO_MINTTY_WARNING").is_some() || !mintty_without_console() {
            return;
        }
        eprintln!(
            "clud: Git Bash/mintty detected without a Windows console; clud cannot run an \
             interactive session here and will use subprocess mode. Run clud from Windows \
             Terminal, or use `winpty clud ...`. (Set CLUD_NO_MINTTY_WARNING=1 to silence.)"
        );
    });
}

/// Raw-byte pump replacing the crossterm event loop on the PTY path.
///
/// Bytes from `stdin_source` flow into the child's PTY via
/// `write_impl`. An `F3Observer` watches the byte stream and — when
/// `hooks.intercept_f3()` is true — counts `\x1bOR` sequences, firing
/// `on_f3_press` once per observed press. The bytes are NOT consumed:
/// the child still receives `\x1bOR` and can handle F3 itself if it
/// wants to. `on_tick` runs every loop iteration regardless of stdin
/// activity so hooks that poll background state (e.g. voice transcripts)
/// still make progress during idle.
///
/// For interactive Windows console input only, the reader normalizes BS
/// (`0x08`) to DEL (`0x7f`) before forwarding. That keeps Backspace aligned
/// with xterm-style TUI expectations even when the console does not emit VT
/// input bytes despite PTY mode requesting them. Non-interactive sources used
/// by tests and pipes are still forwarded unchanged.
///
/// This replaces the old `run_interactive_pty_session` /
/// `run_pty_output_loop` split. The event-loop approach parsed stdin
/// through crossterm's `event::read` — a lossy demultiplexer that dropped
/// every escape sequence it didn't recognize (DSR replies, DA, XTWINOPS,
/// OSC color queries, etc.), which hung child TUIs like codex Ink that
/// write those queries on startup and wait for a reply. See **PR #47**,
/// which made this replacement — not issue #46, which despite being named in
/// that PR's title is "CI: macos-15-intel integration test can't locate
/// mock-agent" and concluded it was not a PTY regression.
///
/// Current scope: stdin forwarding + F3 observation + hook ticks +
/// Ctrl+C + child-exit detection. Resize handling (SIGWINCH on Unix,
/// polled `terminal_size::terminal_size()` on Windows).
///
/// Ctrl-C flow: raw mode turns the keyboard chord into byte `0x03` instead
/// of a terminal signal. For interactive stdin, the pump forwards that byte
/// once, then escalates via `interrupt_pty_process`. On POSIX this sends
/// SIGINT to the child's pgroup and waits up to 2s for exit. On Windows the
/// escalation closes the PTY directly (no extra 0x03 write), because the
/// underlying `send_interrupt_impl` duplicates the byte that stdin already
/// forwarded and makes Ink-based TUIs see a single press as
/// "Ctrl-C twice = exit". The external `interrupted` flag is still honored
/// for non-keyboard interrupts such as OS signals or tests.
pub fn run_raw_pty_pump<H, R>(
    process: &NativePtyProcess,
    interrupted: &AtomicBool,
    hooks: &mut H,
    stdin_source: R,
) -> i32
where
    H: InteractiveHooks,
    R: std::io::Read + Send + 'static,
{
    run_raw_pty_pump_with_extra_rx(process, interrupted, hooks, stdin_source, None)
}

/// Production pump entry that wires a side channel for IDropTarget
/// callbacks (issue #79). Constructs the platform-native resize watcher
/// internally and passes through to `run_raw_pty_pump_full`.
///
/// `extra_rx` chunks are interleaved with stdin chunks and forwarded to
/// the PTY through the same pipeline as real stdin (`forward_user_input`,
/// issue #1350): Backspace normalization, bracketed paste, toast mouse
/// filter and F3. Drop chunks carry no paste markers or ESC bytes, so
/// those steps pass them through unchanged. Only Ctrl+V image expansion
/// is stdin-only.
pub fn run_raw_pty_pump_with_extra_rx<H, R>(
    process: &NativePtyProcess,
    interrupted: &AtomicBool,
    hooks: &mut H,
    stdin_source: R,
    extra_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
) -> i32
where
    H: InteractiveHooks,
    R: std::io::Read + Send + 'static,
{
    run_raw_pty_pump_with_extra_rx_verbose(
        process,
        interrupted,
        hooks,
        stdin_source,
        extra_rx,
        false,
    )
}

/// Production pump entry with optional clud-level diagnostics.
pub fn run_raw_pty_pump_with_extra_rx_verbose<H, R>(
    process: &NativePtyProcess,
    interrupted: &AtomicBool,
    hooks: &mut H,
    stdin_source: R,
    extra_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    verbose: bool,
) -> i32
where
    H: InteractiveHooks,
    R: std::io::Read + Send + 'static,
{
    run_raw_pty_pump_with_extra_rx_verbose_and_graphics(
        process,
        interrupted,
        hooks,
        stdin_source,
        extra_rx,
        verbose,
        None,
    )
}

/// Production pump entry with optional clud-level diagnostics and a PTY
/// header renderer that can redraw/reserve rows after terminal resizes.
pub fn run_raw_pty_pump_with_extra_rx_verbose_and_graphics<H, R>(
    process: &NativePtyProcess,
    interrupted: &AtomicBool,
    hooks: &mut H,
    stdin_source: R,
    extra_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    verbose: bool,
    graphics: Option<GraphicsConfig>,
) -> i32
where
    H: InteractiveHooks,
    R: std::io::Read + Send + 'static,
{
    run_raw_pty_pump_with_extra_rx_verbose_and_graphics_and_filters(
        process,
        interrupted,
        hooks,
        stdin_source,
        extra_rx,
        verbose,
        graphics,
        false,
    )
}

/// Production pump entry that also selects backend-specific output filters.
/// `normalize_bare_lf` chains `codex_lf::CodexLfNormalizer` after the OSC
/// title stripper; pass it only for the Codex backend (#1181).
#[allow(clippy::too_many_arguments)]
pub fn run_raw_pty_pump_with_extra_rx_verbose_and_graphics_and_filters<H, R>(
    process: &NativePtyProcess,
    interrupted: &AtomicBool,
    hooks: &mut H,
    stdin_source: R,
    extra_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    verbose: bool,
    graphics: Option<GraphicsConfig>,
    normalize_bare_lf: bool,
) -> i32
where
    H: InteractiveHooks,
    R: std::io::Read + Send + 'static,
{
    let (resize_tx, resize_rx) = std::sync::mpsc::channel::<(u16, u16)>();
    spawn_os_resize_watcher(resize_tx);
    run_raw_pty_pump_full_verbose(
        process,
        interrupted,
        hooks,
        stdin_source,
        resize_rx,
        extra_rx,
        PumpOptions {
            verbose,
            graphics,
            normalize_bare_lf,
            toasts: None,
            keyboard_enhancement_tracker: None,
        },
    )
}

/// Optional pump features, grouped so the entry-point signature stops growing
/// one positional flag per feature.
#[derive(Default)]
pub struct PumpExtras {
    /// Sixel header to redraw on resize.
    pub graphics: Option<GraphicsConfig>,
    /// Codex-only bare-LF normalizer (#1181).
    pub normalize_bare_lf: bool,
    /// In-terminal toast compositor (#1189).
    pub toasts: Option<crate::toast::compositor::ToastPumpOptions>,
    /// Tracker supplied by `RawTerminalGuard` for child-owned kitty keyboard
    /// protocol frames and terminal modes (#1221, #1704). None outside an
    /// interactive guarded session.
    pub keyboard_enhancement_tracker: Option<Arc<KeyboardEnhancementTracker>>,
}

/// Production pump entry with every optional feature.
pub fn run_raw_pty_pump_with_extras<H, R>(
    process: &NativePtyProcess,
    interrupted: &AtomicBool,
    hooks: &mut H,
    stdin_source: R,
    extra_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    verbose: bool,
    extras: PumpExtras,
) -> i32
where
    H: InteractiveHooks,
    R: std::io::Read + Send + 'static,
{
    let (resize_tx, resize_rx) = std::sync::mpsc::channel::<(u16, u16)>();
    spawn_os_resize_watcher(resize_tx);
    run_raw_pty_pump_full_verbose(
        process,
        interrupted,
        hooks,
        stdin_source,
        resize_rx,
        extra_rx,
        PumpOptions {
            verbose,
            graphics: extras.graphics,
            normalize_bare_lf: extras.normalize_bare_lf,
            toasts: extras.toasts,
            keyboard_enhancement_tracker: extras.keyboard_enhancement_tracker,
        },
    )
}

/// Spawn the platform-native resize-watcher thread that feeds
/// `resize_tx` with `(rows, cols)` whenever the user resizes their
/// terminal window.
///
/// Unix: a SIGWINCH signal handler via `signal-hook`. Zero-latency;
/// the kernel delivers the signal the moment the terminal resizes.
///
/// Windows: 150 ms polling of `crossterm::terminal::size()`. The
/// zero-latency option would be `ReadConsoleInputW` filtering for
/// `WINDOW_BUFFER_SIZE_EVENT`, but that consumes events from the
/// shared console input buffer and races with our stdin reader for
/// keystrokes. Polling avoids the race at the cost of up to 150 ms
/// redraw lag — imperceptible in practice. See the plan file for the
/// deferred zero-latency variant.
fn spawn_os_resize_watcher(resize_tx: std::sync::mpsc::Sender<(u16, u16)>) {
    crate::platform::session::spawn_os_resize_watcher(resize_tx);
}

/// Inner pump entry that takes an explicit resize receiver. Tests use this
/// to inject synthetic resize events without involving platform signal
/// machinery; production wrappers (`run_raw_pty_pump`) construct the
/// channel and spawn a platform-native resize producer thread.
pub fn run_raw_pty_pump_with_resize_rx<H, R>(
    process: &NativePtyProcess,
    interrupted: &AtomicBool,
    hooks: &mut H,
    stdin_source: R,
    resize_rx: std::sync::mpsc::Receiver<(u16, u16)>,
) -> i32
where
    H: InteractiveHooks,
    R: std::io::Read + Send + 'static,
{
    run_raw_pty_pump_full(process, interrupted, hooks, stdin_source, resize_rx, None)
}

/// Most-general pump entry. See `run_raw_pty_pump_with_extra_rx` for
/// the public-facing version that constructs the resize receiver.
pub fn run_raw_pty_pump_full<H, R>(
    process: &NativePtyProcess,
    interrupted: &AtomicBool,
    hooks: &mut H,
    stdin_source: R,
    resize_rx: std::sync::mpsc::Receiver<(u16, u16)>,
    extra_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
) -> i32
where
    H: InteractiveHooks,
    R: std::io::Read + Send + 'static,
{
    run_raw_pty_pump_full_verbose(
        process,
        interrupted,
        hooks,
        stdin_source,
        resize_rx,
        extra_rx,
        PumpOptions::default(),
    )
}

/// Test-only seam: identical to [`run_raw_pty_pump_full`] but takes the
/// destination writer for filtered child output instead of hardcoding
/// `io::stdout()`. Lets integration tests inject a slow/counting sink
/// to prove the writer-thread decoupling from issue #538 (stdout flush
/// no longer gates stdin forwarding, output coalesces into O(1)
/// flushes). Not part of the stable public API — production code
/// always goes through the `io::stdout()`-bound `run_raw_pty_pump*`
/// family above.
/// Test-only seam (#1189): the writer-injection pump with a toast
/// compositor, so PTY integration tests on every platform can assert the
/// composited byte stream a real terminal would receive.
#[doc(hidden)]
pub fn run_raw_pty_pump_with_toasts_for_test<H, R, W>(
    process: &NativePtyProcess,
    interrupted: &AtomicBool,
    hooks: &mut H,
    stdin_source: R,
    resize_rx: std::sync::mpsc::Receiver<(u16, u16)>,
    writer: W,
    toasts: crate::toast::compositor::ToastPumpOptions,
) -> i32
where
    H: InteractiveHooks,
    R: std::io::Read + Send + 'static,
    W: std::io::Write + Send,
{
    run_raw_pty_pump_full_verbose_with_writer(
        process,
        interrupted,
        hooks,
        stdin_source,
        resize_rx,
        None,
        PumpOptions {
            toasts: Some(toasts),
            ..PumpOptions::default()
        },
        writer,
    )
}

#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn run_raw_pty_pump_full_with_writer_for_test<H, R, W>(
    process: &NativePtyProcess,
    interrupted: &AtomicBool,
    hooks: &mut H,
    stdin_source: R,
    resize_rx: std::sync::mpsc::Receiver<(u16, u16)>,
    extra_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    writer: W,
) -> i32
where
    H: InteractiveHooks,
    R: std::io::Read + Send + 'static,
    W: std::io::Write + Send,
{
    run_raw_pty_pump_full_verbose_with_writer(
        process,
        interrupted,
        hooks,
        stdin_source,
        resize_rx,
        extra_rx,
        PumpOptions::default(),
        writer,
    )
}

#[derive(Default)]
struct PumpOptions {
    verbose: bool,
    graphics: Option<GraphicsConfig>,
    /// In-terminal toast compositor for the writer thread (#1189).
    toasts: Option<crate::toast::compositor::ToastPumpOptions>,
    /// Chain `codex_lf::CodexLfNormalizer` after the OSC stripper on the
    /// output reader (#1181). Only the Codex backend sets this; see that
    /// module for why the filter must never run for other TUIs.
    normalize_bare_lf: bool,
    keyboard_enhancement_tracker: Option<Arc<KeyboardEnhancementTracker>>,
}

/// Idle cadence of the pump's main loop (#691).
///
/// Every input source — stdin, `extra_rx`, resize, and the output reader's
/// close notice — arrives as a [`PumpEvent`] on one channel, and
/// `recv_timeout` wakes the moment one is sent (mpsc's receiver is
/// condvar-backed). So this bound never delays a keystroke or a resize; it
/// only sets how often clud re-checks what cannot send an event: the
/// `interrupted` flag (set from a signal handler), `on_tick` hooks such as
/// the voice-transcript drain, and child exit on Windows, where ConPTY keeps
/// the output pipe open after the child is gone. 50 ms matches the output
/// reader's own poll and replaces the old 5 ms stdin poll, which alone was
/// 200 idle wakeups a second per session.
const PUMP_TICK: std::time::Duration = std::time::Duration::from_millis(50);

/// Whether the pump must answer the child's `ESC[6n` cursor queries itself.
///
/// ConPTY sends that query when it starts and holds the child until a
/// terminal replies (#1310). With a real interactive console on stdin, the
/// terminal answers through the pump's stdin path, and a clud stub would be
/// a second, wrong reply (#31). With anything else — piped stdin, a test's
/// in-memory reader — nothing would ever answer, and the child never runs.
/// POSIX PTYs send no such query.
fn should_answer_cursor_queries(interactive_real_stdin: bool) -> bool {
    crate::platform::host::WINDOWS && !interactive_real_stdin
}

/// Environment switch that forces the pump's verbose trace on (#1310).
pub const PUMP_TRACE_ENV: &str = "CLUD_PTY_PUMP_TRACE";

/// How long a partial SGR mouse report held by the toast mouse filter waits
/// for its continuation (#1189). The paste normalizer's own held bytes follow
/// `BracketedPasteNormalizer::flush_due_in` (#1445, #1717). Only armed while
/// bytes are pending, so it never raises the idle wakeup rate.
const INPUT_PENDING_FLUSH: std::time::Duration = std::time::Duration::from_millis(5);

/// One unit of work for the pump's main loop. Every producer feeds the same
/// channel so the loop blocks on a single `recv_timeout` instead of polling
/// each source in turn (#691).
enum PumpEvent {
    /// Bytes from the byte-stream stdin reader.
    Stdin(Vec<u8>),
    /// Bytes from `extra_rx` (the Windows `console_input` reader or the OLE
    /// drag-drop callback).
    Extra(Vec<u8>),
    /// A terminal resize as `(rows, cols)`.
    Resize(u16, u16),
    /// The output reader saw the PTY close.
    ReaderClosed,
}

/// Forward every message from `rx` into the pump's event channel, wrapped by
/// `wrap`. The thread blocks in `recv` — it costs no idle wakeups — and exits
/// once either side disconnects.
fn spawn_pump_forwarder<T, F>(
    name: &str,
    rx: std::sync::mpsc::Receiver<T>,
    tx: std::sync::mpsc::Sender<PumpEvent>,
    wrap: F,
) where
    T: Send + 'static,
    F: Fn(T) -> PumpEvent + Send + 'static,
{
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            while let Ok(message) = rx.recv() {
                if tx.send(wrap(message)).is_err() {
                    break;
                }
            }
        });
    if let Err(err) = spawned {
        eprintln!("[clud] warning: failed to start pty pump forwarder {name}: {err}");
    }
}

/// Blocking read timeout used by the dedicated output-reader thread
/// (issue #538). This thread never gates stdin forwarding — it feeds a
/// dedicated writer thread over an unbounded channel, so a stalled
/// terminal write can never delay it. The value only affects shutdown
/// responsiveness (how quickly the reader notices `stop_reader`) and
/// idle CPU usage.
const OUTPUT_READER_POLL_SECS: f64 = 0.05;

fn run_raw_pty_pump_full_verbose<H, R>(
    process: &NativePtyProcess,
    interrupted: &AtomicBool,
    hooks: &mut H,
    stdin_source: R,
    resize_rx: std::sync::mpsc::Receiver<(u16, u16)>,
    extra_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    options: PumpOptions,
) -> i32
where
    H: InteractiveHooks,
    R: std::io::Read + Send + 'static,
{
    run_raw_pty_pump_full_verbose_with_writer(
        process,
        interrupted,
        hooks,
        stdin_source,
        resize_rx,
        extra_rx,
        options,
        io::stdout(),
    )
}

/// The toast hit targets the mouse filter checks, snapshotted per chunk.
#[derive(Debug, Clone, Copy)]
struct ToastHitTargets {
    close: Option<crate::toast::text_tier::CellRect>,
    cpu: Option<crate::toast::text_tier::CellRect>,
    hover_armed: bool,
}

/// Issue #1350: prepares a `console_input` / drag-drop chunk for the shared
/// user-input pipeline. On Windows the `console_input` reader replaces the
/// byte-stream reader, so the Backspace normalization that reader applies
/// must happen here instead.
fn extra_chunk_for_pipeline(chunk: &[u8], normalize_console_stdin: bool) -> Vec<u8> {
    let mut chunk = chunk.to_vec();
    if normalize_console_stdin {
        normalize_interactive_console_stdin_chunk(&mut chunk);
    }
    chunk
}

/// The pure byte transform every user-input chunk goes through before it
/// reaches the PTY: bracketed-paste normalization, then the toast mouse
/// filter (#1189) when a toast is armed.
///
/// Drag-drop chunks (`dnd::injectors::join_paths_for_injection`) are plain
/// newline-joined paths with no bracketed-paste markers, so the paste step
/// passes them through byte-for-byte, as does the mouse filter (no ESC).
fn filter_user_input_chunk(
    chunk: &[u8],
    paste: &mut BracketedPasteNormalizer,
    mouse: &mut crate::toast::mouse::MouseFilter,
    targets: Option<ToastHitTargets>,
) -> crate::toast::mouse::MouseResult {
    let outgoing = paste.process(chunk);
    match targets {
        Some(t) => mouse.process(&outgoing, t.close, t.cpu, t.hover_armed),
        None => crate::toast::mouse::MouseResult {
            bytes: outgoing,
            ..Default::default()
        },
    }
}

fn pending_user_input_wait(
    until_tick: std::time::Duration,
    now: std::time::Instant,
    paste: &BracketedPasteNormalizer,
    mouse: &crate::toast::mouse::MouseFilter,
    toast_armed: bool,
) -> std::time::Duration {
    let mut wait = until_tick;
    if let Some(due) = paste.flush_due_in(now) {
        wait = wait.min(due);
    }
    if toast_armed && mouse.has_pending() {
        wait = wait.min(INPUT_PENDING_FLUSH);
    }
    wait
}

/// Release what the paste and mouse filters hold, in input order, once it is
/// due. A held paste prefix must still pass through the toast mouse filter
/// before reaching the child; its partial report is then released here too.
fn flush_pending_user_input(
    now: std::time::Instant,
    paste: &mut BracketedPasteNormalizer,
    mouse: &mut crate::toast::mouse::MouseFilter,
    targets: Option<ToastHitTargets>,
) -> Vec<u8> {
    let pending = paste.flush_pending_if_due(now);
    match targets {
        Some(targets) => {
            let mut bytes = mouse
                .process(&pending, targets.close, targets.cpu, targets.hover_armed)
                .bytes;
            bytes.extend(mouse.flush_pending());
            bytes
        }
        None => pending,
    }
}

/// Issue #1350: the single post-read pipeline shared by the byte-stream
/// stdin arm and the `console_input` / drag-drop (`extra_rx`) arm, so the
/// two input sources cannot drift apart again: paste + toast mouse filter,
/// toast side effects, the PTY write, then F3 voice-hotkey observation.
#[allow(clippy::too_many_arguments)]
fn forward_user_input<H: InteractiveHooks>(
    process: &NativePtyProcess,
    hooks: &mut H,
    observer: &mut F3Observer,
    paste: &mut BracketedPasteNormalizer,
    mouse: &mut crate::toast::mouse::MouseFilter,
    toast_input: Option<&crate::toast::compositor::ToastInput>,
    toast_hub: Option<&crate::toast::ToastHub>,
    chunk: &[u8],
    source: &str,
) {
    let targets = toast_input.map(|input| ToastHitTargets {
        close: input.close_rect(),
        cpu: input.cpu_rect(),
        hover_armed: input.cpu_hover_armed(),
    });
    let result = filter_user_input_chunk(chunk, paste, mouse, targets);
    if result.dismissed {
        if let Some(hub) = toast_hub {
            if targets.and_then(|target| target.cpu).is_some() {
                hub.dismiss_visible_except(
                    std::time::Instant::now(),
                    crate::cpu_banner::CPU_TOAST_KEY,
                );
            } else {
                hub.dismiss_visible(std::time::Instant::now());
            }
        }
    }
    if let Some(input) = toast_input {
        if let Some(hovering) = result.cpu_hover {
            input.set_cpu_hover(hovering);
        }
    }
    let write_result = if result.bytes.is_empty() {
        Ok(())
    } else {
        process.write_impl(&result.bytes, false)
    };
    if let Err(err) = write_result {
        eprintln!("[clud] warning: failed to forward {source} to pty: {}", err);
    } else if hooks.intercept_f3() {
        // F3 detection runs over the user input (after Ctrl+V image
        // expansion on the stdin path) but before any backend write side
        // effects. A press inside paste text is unusual but should keep
        // detection symmetry with raw forwarding.
        let events = observer.observe(chunk);
        let mut sink = NativePtyProcessSink::new(process);
        for _ in 0..events.presses {
            if let Err(err) = hooks.on_f3_press(&mut sink) {
                eprintln!("[clud] warning: voice F3 press hook failed: {}", err);
            }
        }
        for _ in 0..events.releases {
            if let Err(err) = hooks.on_f3_release(&mut sink) {
                eprintln!("[clud] warning: voice F3 release hook failed: {}", err);
            }
        }
    }
}

/// Drains `rx`, coalescing every chunk already queued into one
/// `write_all` + one `flush` per wakeup, until the channel disconnects
/// (draining and flushing anything left one last time before
/// returning). Shared by the pump's dedicated writer thread and by
/// unit tests that exercise the coalescing behavior in isolation,
/// without spinning up a full PTY (issue #538).
/// Same as `run_raw_pty_pump_full_verbose`, but takes the destination
/// writer for filtered child output as a parameter instead of
/// hardcoding `io::stdout()`. Production callers go through the
/// `io::stdout()`-bound wrapper above; tests inject a slow/counting
/// sink to exercise the writer-thread decoupling (issue #538).
///
/// Architecture (issue #538): output reading/filtering/writing used to
/// happen inline in the same loop turn that forwards stdin, so a slow
/// terminal `flush()` delayed keystroke delivery, each chunk got its
/// own write+flush (no coalescing), and the loop's cadence for
/// checking stdin was floored at the output read's 10ms timeout. Now:
///
/// * A dedicated **reader thread** calls `read_chunk_impl` in a loop,
///   OSC-strips each chunk, coalesces any additional chunks already
///   queued (non-blocking `Some(0.0)` drain) into one buffer, and
///   sends it over an *unbounded* channel — `send` never blocks, so a
///   slow writer can never stall this thread.
/// * A dedicated **writer thread** (`run_output_writer`) blocks on
///   that channel, drains everything else pending, and does exactly
///   one `write_all` + one `flush` per wakeup — turning a burst of N
///   chunks into O(1) syscalls.
/// * The **main thread** (this function body) only handles resize,
///   `extra_rx`, stdin forwarding, hook ticks, and exit/interrupt
///   detection. Every input source feeds one [`PumpEvent`] channel and
///   the thread blocks on `event_rx.recv_timeout` until the next event
///   or the [`PUMP_TICK`] deadline (#691), so keystroke forwarding wakes
///   as soon as a chunk is queued and an idle session wakes 20 times a
///   second instead of 200.
///
/// All three "threads" share `process: &NativePtyProcess` (its
/// handles/reader state are internally `Mutex`-guarded and already
/// designed for concurrent access — see `daemon/worker.rs`'s reader +
/// writer thread pair for the established precedent). `thread::scope`
/// is used instead of `thread::spawn` because `process` is a borrow,
/// not an owned `Arc`; the scope block joins both worker threads
/// before returning, which is also how "flush remaining chunks on
/// exit" is guaranteed — the writer only stops once its channel
/// disconnects, i.e. after the reader has stopped and sent everything
/// it read.
#[allow(clippy::too_many_arguments)]
fn run_raw_pty_pump_full_verbose_with_writer<H, R, W>(
    process: &NativePtyProcess,
    interrupted: &AtomicBool,
    hooks: &mut H,
    stdin_source: R,
    resize_rx: std::sync::mpsc::Receiver<(u16, u16)>,
    extra_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    mut options: PumpOptions,
    writer: W,
) -> i32
where
    H: InteractiveHooks,
    R: std::io::Read + Send + 'static,
    W: std::io::Write + Send,
{
    use std::sync::mpsc;

    // #1310: `CLUD_PTY_PUMP_TRACE=1` turns on the pump's verbose lines for
    // any caller, so a CI run can show where a Windows PTY test stalls.
    options.verbose |= std::env::var_os(PUMP_TRACE_ENV).is_some_and(|value| value != "0");
    let (event_tx, event_rx) = mpsc::channel::<PumpEvent>();
    let interactive_real_stdin = stdin_source_is_real_stdin::<R>() && terminals_are_interactive();
    let normalize_console_stdin =
        should_normalize_interactive_console_stdin(interactive_real_stdin);
    start_pump_input_sources(
        stdin_source,
        resize_rx,
        extra_rx,
        &event_tx,
        interactive_real_stdin,
        normalize_console_stdin,
        options.verbose,
    );

    // Issue #538: output reading/filtering/writing now happens on two
    // dedicated threads (reader + writer) instead of inline in this
    // loop, so a slow terminal `flush()` can never delay stdin
    // forwarding below. `thread::scope` borrows `process` safely (it's
    // `&NativePtyProcess`, not an owned `Arc`) and joins both threads
    // before returning — which is also what guarantees the writer
    // flushes any remaining chunks before the pump exits.
    let stop_reader = AtomicBool::new(false);
    let reader_closed = AtomicBool::new(false);

    let exit_code = std::thread::scope(|scope| {
        let (output_tx, output_rx) = mpsc::channel::<OutputMsg>();
        // #1189: the compositor lives on the writer thread; the stdin path
        // shares its close-button hit rect and the hub for dismissal.
        let compositor = options
            .toasts
            .clone()
            .map(crate::toast::compositor::Compositor::new);
        let mut input = PumpInputState::new(
            compositor.as_ref().map(|c| c.input()),
            options
                .toasts
                .as_ref()
                .map(|toasts| std::sync::Arc::clone(&toasts.hub)),
            output_tx.clone(),
            interactive_real_stdin,
            normalize_console_stdin,
        );
        let stop_reader = &stop_reader;
        let reader_closed = &reader_closed;

        // Writer thread: coalesces every chunk already queued into one
        // write_all + one flush per wakeup (see `run_output_writer`).
        // Exits once `output_tx` is dropped AND the queue is drained —
        // i.e. after the reader thread has stopped and sent everything
        // it had. That ordering is the "flush remaining chunks first"
        // shutdown guarantee.
        scope.spawn(move || {
            run_output_writer_composited(output_rx, writer, compositor);
        });

        // Reader thread: never blocks the writer or the main loop.
        // `output_tx.send` on an unbounded channel never blocks, so a
        // stalled terminal write on the writer thread cannot delay
        // this thread from continuing to read, and cannot delay the
        // main thread's stdin forwarding (which no longer touches
        // output at all). See `PtyOutputReader`.
        let reader = PtyOutputReader::new(process, &options, interactive_real_stdin);
        let closed_tx = event_tx.clone();
        scope.spawn(move || reader.run(stop_reader, reader_closed, &output_tx, &closed_tx));

        // The main thread holds `event_tx` for the whole loop, so the channel
        // never disconnects: after stdin EOF `recv_timeout` still waits out
        // its timeout instead of returning at once and spinning a core.
        let _event_channel_open = &event_tx;
        // #1310: stop the reader even when a hook panics out of this loop.
        // `thread::scope` joins the reader before re-raising the panic, and
        // on Windows ConPTY never closes its pipe, so a reader that is never
        // told to stop would keep the unwind waiting forever.
        let _stop_reader_on_unwind = StopReaderOnDrop(stop_reader);
        let mut next_tick = std::time::Instant::now() + PUMP_TICK;
        let exit_code = loop {
            let until_tick = next_tick.saturating_duration_since(std::time::Instant::now());
            let wait = input.wait_for(until_tick);
            // One blocking wait covers every input source (#691). Events
            // are handled in arrival order; a keystroke, resize, or
            // drag-drop wakes the loop immediately.
            let event = event_rx.recv_timeout(wait);
            if let Some(code) = input.handle_event(event, process, hooks, interrupted, &options) {
                break code;
            }
            if std::time::Instant::now() >= next_tick {
                next_tick = std::time::Instant::now() + PUMP_TICK;
            }
            if let Some(code) =
                tick_and_check_pump_exit(process, hooks, reader_closed, interrupted, &options)
            {
                break code;
            }
        };

        stop_reader.store(true, Ordering::Release);
        if options.verbose {
            verbose_log::log(format_args!(
                "[clud] pty pump: loop exited code {exit_code}; joining reader/writer"
            ));
        }
        exit_code
    });
    if options.verbose {
        verbose_log::log(format_args!("[clud] pty pump: returned {exit_code}"));
    }
    exit_code
}

/// Wire every input source into the pump's event channel: the byte-stream
/// stdin reader (when it is the keyboard source), resize, and `extra_rx`.
fn start_pump_input_sources<R>(
    stdin_source: R,
    resize_rx: std::sync::mpsc::Receiver<(u16, u16)>,
    extra_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    event_tx: &std::sync::mpsc::Sender<PumpEvent>,
    interactive_real_stdin: bool,
    normalize_console_stdin: bool,
    verbose: bool,
) where
    R: std::io::Read + Send + 'static,
{
    // Issue #188: when an `extra_rx` is wired and we're on Windows with
    // an interactive real-stdin console, the `console_input`
    // `ReadConsoleInputW` reader is feeding that channel and is the
    // authoritative source of console bytes (including Shift+Enter →
    // `\n`). Spawning the byte-stream stdin reader below would race
    // with it on the same STDIN console queue — `ReadFile` strips
    // modifier state, so a stolen Shift+Enter surfaces as `\r`. Skip
    // it in that exact case so `console_input` is the sole consumer.
    let spawn_byte_stream_stdin_reader =
        should_spawn_byte_stream_stdin_reader(interactive_real_stdin, extra_rx.is_some());
    if verbose {
        verbose_log::log(format_args!(
            "[clud] pty pump: start interactive_stdin={} normalize_console_stdin={} \
             spawn_byte_stream_stdin_reader={}",
            interactive_real_stdin, normalize_console_stdin, spawn_byte_stream_stdin_reader
        ));
    }

    if spawn_byte_stream_stdin_reader {
        spawn_byte_stream_stdin_reader_thread(
            stdin_source,
            event_tx.clone(),
            normalize_console_stdin,
        );
    } else {
        // `console_input` is the sole consumer via `extra_rx`; nothing
        // reads the byte-stream source.
        drop(stdin_source);
    }
    // #691: resize and `extra_rx` join the same event channel, so the main
    // loop wakes on whichever arrives first instead of polling each.
    spawn_pump_forwarder(
        "clud-pty-resize",
        resize_rx,
        event_tx.clone(),
        |(rows, cols)| PumpEvent::Resize(rows, cols),
    );
    if let Some(extra_rx) = extra_rx {
        spawn_pump_forwarder(
            "clud-pty-extra-input",
            extra_rx,
            event_tx.clone(),
            PumpEvent::Extra,
        );
    }
}

/// Detached reader: pumps `stdin_source` → channel until EOF or error.
/// Detached (not joined) so a blocked `read()` on real stdin doesn't
/// wedge shutdown when the child exits — the process is terminating
/// anyway. See Step 12.
fn spawn_byte_stream_stdin_reader_thread<R>(
    stdin_source: R,
    stdin_tx: std::sync::mpsc::Sender<PumpEvent>,
    normalize_console_stdin: bool,
) where
    R: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut reader = stdin_source;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    let mut chunk = buf[..n].to_vec();
                    if normalize_console_stdin {
                        normalize_interactive_console_stdin_chunk(&mut chunk);
                    }
                    if stdin_tx.send(PumpEvent::Stdin(chunk)).is_err() {
                        break; // Main thread dropped the receiver → exit.
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// Sets the pump's reader-stop flag when dropped, including on unwind (#1310).
struct StopReaderOnDrop<'a>(&'a AtomicBool);

impl Drop for StopReaderOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The PTY output reader thread's state (#538).
///
/// Calls `read_chunk_impl` in a loop and filters each chunk before it
/// reaches the terminal. Strip OSC 0/2 (window-title) sequences from the
/// child's output before they reach the terminal — the library also keeps an
/// un-stripped copy in its own `chunks` queue (used by callers like
/// capture/replay), but only the bytes sent here actually reach the user's
/// terminal. `main.rs` set `process.set_echo(false)` so the library's
/// built-in stdout writer is silent — we own forwarding now.
struct PtyOutputReader<'p> {
    process: &'p NativePtyProcess,
    osc_strip: OscTitleStripper,
    /// Codex-only (#1181): rewrites bare LF to CRLF after the OSC strip.
    /// `None` for every other backend so the filter can never touch a TUI
    /// that relies on bare LF inside a scroll region.
    lf_normalize: Option<crate::codex_lf::CodexLfNormalizer>,
    keyboard_enhancement_tracker: Option<Arc<KeyboardEnhancementTracker>>,
    /// #1310 / #1347 / #1702: see `should_answer_cursor_queries` and
    /// `terminal_queries`.
    query_scanner: Option<crate::terminal_queries::TerminalQueryScanner>,
    verbose: bool,
}

impl<'p> PtyOutputReader<'p> {
    fn new(
        process: &'p NativePtyProcess,
        options: &PumpOptions,
        interactive_real_stdin: bool,
    ) -> Self {
        Self {
            process,
            osc_strip: OscTitleStripper::new(),
            lf_normalize: options
                .normalize_bare_lf
                .then(crate::codex_lf::CodexLfNormalizer::new),
            keyboard_enhancement_tracker: options.keyboard_enhancement_tracker.clone(),
            query_scanner: should_answer_cursor_queries(interactive_real_stdin)
                .then(crate::terminal_queries::TerminalQueryScanner::default),
            verbose: options.verbose,
        }
    }

    fn observe_and_filter(&mut self, chunk: &[u8]) -> Vec<u8> {
        if let Some(tracker) = &self.keyboard_enhancement_tracker {
            tracker.observe(chunk);
        }
        if let Some(scanner) = self.query_scanner.as_mut() {
            let replies = scanner.replies(chunk);
            if !replies.is_empty() {
                if self.verbose {
                    verbose_log::log("[clud] pty pump: answering child terminal query");
                }
                let _ = self.process.write_impl(&replies, false);
            }
        }
        let stripped = self.osc_strip.process(chunk);
        match self.lf_normalize.as_mut() {
            Some(lf) => lf.process(&stripped),
            None => stripped,
        }
    }

    fn run(
        mut self,
        stop_reader: &AtomicBool,
        reader_closed: &AtomicBool,
        output_tx: &std::sync::mpsc::Sender<OutputMsg>,
        closed_tx: &std::sync::mpsc::Sender<PumpEvent>,
    ) {
        let process = self.process;
        loop {
            if stop_reader.load(Ordering::Acquire) {
                // Final non-blocking drain so a chunk that arrived
                // right before shutdown isn't lost.
                while let Ok(Some(chunk)) = process.read_chunk_impl(Some(0.0)) {
                    let filtered = self.observe_and_filter(&chunk);
                    if !filtered.is_empty() {
                        let _ = output_tx.send(OutputMsg::Child(filtered));
                    }
                }
                break;
            }
            match process.read_chunk_impl(Some(OUTPUT_READER_POLL_SECS)) {
                Ok(Some(chunk)) => {
                    let mut filtered = self.observe_and_filter(&chunk);
                    // Coalesce clud-side: drain whatever else is
                    // already queued without blocking, so a
                    // chatty child's burst becomes one send (and,
                    // downstream, one write+flush) instead of one
                    // per chunk.
                    while let Ok(Some(more)) = process.read_chunk_impl(Some(0.0)) {
                        filtered.extend_from_slice(&self.observe_and_filter(&more));
                    }
                    if !filtered.is_empty() {
                        let _ = output_tx.send(OutputMsg::Child(filtered));
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    reader_closed.store(true, Ordering::Release);
                    // Wake the main loop now rather than at its next tick.
                    let _ = closed_tx.send(PumpEvent::ReaderClosed);
                    break;
                }
            }
        }
    }
}

/// The pump main thread's user-input state: everything a [`PumpEvent`]
/// handler reads or mutates between wakeups.
struct PumpInputState {
    observer: F3Observer,
    // #1703: one per input stream, so a CSI u Ctrl+C split across two reads
    // of that stream is still seen.
    extra_interrupt: InterruptScanner,
    stdin_interrupt: InterruptScanner,
    // Issue #63 / #79: bracketed-paste passes through the PTY pump as
    // raw bytes. When the user drags a file onto the terminal, the
    // terminal emits `\x1b[200~ <path-shaped string> \x1b[201~`. We
    // normalize that path BEFORE forwarding so all backends see a
    // canonical form, regardless of which terminal produced the drop.
    paste: BracketedPasteNormalizer,
    // #1189: swallows clicks on a visible toast's close button; a byte-exact
    // pass-through whenever no toast is armed.
    mouse: crate::toast::mouse::MouseFilter,
    toast_input: Option<Arc<crate::toast::compositor::ToastInput>>,
    toast_hub: Option<Arc<crate::toast::ToastHub>>,
    resize_out: std::sync::mpsc::Sender<OutputMsg>,
    interactive_real_stdin: bool,
    normalize_console_stdin: bool,
    interrupt_on_ctrl_c_byte: bool,
}

impl PumpInputState {
    fn new(
        toast_input: Option<Arc<crate::toast::compositor::ToastInput>>,
        toast_hub: Option<Arc<crate::toast::ToastHub>>,
        resize_out: std::sync::mpsc::Sender<OutputMsg>,
        interactive_real_stdin: bool,
        normalize_console_stdin: bool,
    ) -> Self {
        Self {
            observer: F3Observer::new(),
            extra_interrupt: InterruptScanner::default(),
            stdin_interrupt: InterruptScanner::default(),
            paste: BracketedPasteNormalizer::new(),
            mouse: crate::toast::mouse::MouseFilter::new(),
            toast_input,
            toast_hub,
            resize_out,
            interactive_real_stdin,
            normalize_console_stdin,
            interrupt_on_ctrl_c_byte: interactive_real_stdin,
        }
    }

    fn wait_for(&self, until_tick: std::time::Duration) -> std::time::Duration {
        pending_user_input_wait(
            until_tick,
            std::time::Instant::now(),
            &self.paste,
            &self.mouse,
            self.toast_input.is_some(),
        )
    }

    /// Handle one wakeup of the main loop; `Some(code)` ends the pump.
    fn handle_event<H: InteractiveHooks>(
        &mut self,
        event: Result<PumpEvent, std::sync::mpsc::RecvTimeoutError>,
        process: &NativePtyProcess,
        hooks: &mut H,
        interrupted: &AtomicBool,
        options: &PumpOptions,
    ) -> Option<i32> {
        use std::sync::mpsc::RecvTimeoutError;
        match event {
            Ok(PumpEvent::Resize(rows, cols)) => self.on_resize(process, rows, cols, options),
            Ok(PumpEvent::Extra(chunk)) => {
                return self.on_extra(process, hooks, &chunk, options.verbose);
            }
            Ok(PumpEvent::Stdin(chunk)) => {
                return self.on_stdin(process, hooks, &chunk, interrupted, options.verbose);
            }
            // The reader already set `reader_closed`; the exit check
            // below acts on it.
            Ok(PumpEvent::ReaderClosed) => {}
            Err(RecvTimeoutError::Timeout) => self.flush_due_input(process),
            Err(RecvTimeoutError::Disconnected) => {
                // Unreachable while `_event_channel_open` holds a
                // sender; sleep rather than spin if that ever changes.
                std::thread::sleep(PUMP_TICK);
            }
        }
        None
    }

    fn on_resize(&self, process: &NativePtyProcess, rows: u16, cols: u16, options: &PumpOptions) {
        let pty_rows = options
            .graphics
            .as_ref()
            .map(|config| redraw_graphics_header_for_resize(config, rows, cols, options.verbose))
            .unwrap_or(rows);
        if let Err(err) = resize_pty(process, pty_rows, cols) {
            eprintln!("[clud] warning: failed to resize pty: {}", err);
        }
        if self.toast_input.is_some() {
            let _ = self.resize_out.send(OutputMsg::Resize {
                rows: pty_rows,
                cols,
            });
        }
    }

    fn forward<H: InteractiveHooks>(
        &mut self,
        process: &NativePtyProcess,
        hooks: &mut H,
        chunk: &[u8],
        source: &str,
    ) {
        forward_user_input(
            process,
            hooks,
            &mut self.observer,
            &mut self.paste,
            &mut self.mouse,
            self.toast_input.as_deref(),
            self.toast_hub.as_deref(),
            chunk,
            source,
        );
    }

    /// A side-channel chunk (Windows `console_input` keyboard or
    /// the drag-drop OLE callback). Issue #1350: on Windows this
    /// is the *only* keyboard source, so it runs the same
    /// `forward_user_input` pipeline as stdin (Backspace
    /// normalization, bracketed paste, toast mouse filter, F3).
    /// Ctrl+V image expansion stays stdin-only.
    ///
    /// The 0x03 byte check is required on Windows: when the
    /// `console_input` reader (issue #141 / PR #144) is active,
    /// it turns off `ENABLE_PROCESSED_INPUT` so the OS no
    /// longer fires a `CTRL_C_EVENT` for Ctrl-C. The press
    /// arrives instead as a KEY_EVENT whose translated 0x03
    /// byte is delivered via this channel — without the check,
    /// clud forwards it to the child but never observes the
    /// interrupt itself.
    fn on_extra<H: InteractiveHooks>(
        &mut self,
        process: &NativePtyProcess,
        hooks: &mut H,
        chunk: &[u8],
        verbose: bool,
    ) -> Option<i32> {
        // Unlike stdin, extra_rx is by construction
        // always user-driven (keyboard via
        // console_input_rx on Windows, or OLE drag-drop
        // callback) — never a piped test fixture — so we
        // don't need the `interrupt_on_ctrl_c_byte` gate
        // that skips 0x03 detection on piped stdin.
        let requested_interrupt = self.extra_interrupt.requests_interrupt(chunk);
        let chunk = extra_chunk_for_pipeline(chunk, self.normalize_console_stdin);
        self.forward(process, hooks, &chunk, "console input");
        if !requested_interrupt {
            return None;
        }
        if verbose {
            verbose_log::log("[clud] pty pump: interrupt via extra_rx Ctrl+C byte");
        }
        Some(interrupt_pty_process(process, verbose))
    }

    fn on_stdin<H: InteractiveHooks>(
        &mut self,
        process: &NativePtyProcess,
        hooks: &mut H,
        chunk: &[u8],
        interrupted: &AtomicBool,
        verbose: bool,
    ) -> Option<i32> {
        let requested_interrupt =
            self.interrupt_on_ctrl_c_byte && self.stdin_interrupt.requests_interrupt(chunk);
        let chunk = if self.interactive_real_stdin {
            crate::paste_image::expand_ctrl_v_bytes(chunk, || {
                crate::paste_image::handle_clipboard().ok().flatten()
            })
        } else {
            std::borrow::Cow::Borrowed(chunk)
        };
        self.forward(process, hooks, chunk.as_ref(), "stdin");

        if !(requested_interrupt || interrupted.load(Ordering::SeqCst)) {
            return None;
        }
        if verbose {
            let source = if requested_interrupt {
                "stdin Ctrl+C byte"
            } else {
                "interrupt flag"
            };
            verbose_log::log(format_args!("[clud] pty pump: interrupt via {source}"));
        }
        Some(interrupt_pty_process(process, verbose))
    }

    /// #1445 / #1717: release a held prefix once it is due (a
    /// lone Esc fast, a partial terminal report only after a
    /// longer wait), and any toast mouse prefix after idle.
    fn flush_due_input(&mut self, process: &NativePtyProcess) {
        let targets = self.toast_input.as_ref().map(|input| ToastHitTargets {
            close: input.close_rect(),
            cpu: input.cpu_rect(),
            hover_armed: input.cpu_hover_armed(),
        });
        let pending = flush_pending_user_input(
            std::time::Instant::now(),
            &mut self.paste,
            &mut self.mouse,
            targets,
        );
        if !pending.is_empty() {
            let _ = process.write_impl(&pending, false);
        }
    }
}

/// Run the hooks' tick, then check every exit condition the event channel
/// cannot signal; `Some(code)` ends the pump.
fn tick_and_check_pump_exit<H: InteractiveHooks>(
    process: &NativePtyProcess,
    hooks: &mut H,
    reader_closed: &AtomicBool,
    interrupted: &AtomicBool,
    options: &PumpOptions,
) -> Option<i32> {
    {
        let mut sink = NativePtyProcessSink::new(process);
        if let Err(err) = hooks.on_tick(&mut sink) {
            eprintln!("[clud] warning: interactive hook tick failed: {}", err);
        }
    }

    if reader_closed.load(Ordering::Acquire) {
        if options.verbose {
            verbose_log::log("[clud] pty pump: output reader observed pty closed");
        }
        return Some(reap_pty_exit(process));
    }

    if let Ok(Some(code)) =
        running_process::pty::poll_pty_process(&process.handles, &process.returncode)
    {
        if options.verbose {
            verbose_log::log(format_args!("[clud] pty pump: child exited code {code}"));
        }
        return Some(code);
    }

    if interrupted.load(Ordering::SeqCst) {
        if options.verbose {
            verbose_log::log("[clud] pty pump: interrupt flag observed");
        }
        return Some(interrupt_pty_process(process, options.verbose));
    }
    None
}

/// `extra_rx` already wired, the `console_input::ReadConsoleInputW`
/// worker is the authoritative source of console bytes — including the
/// modifier-aware Shift+Enter → `\n` translation. Spawning the
/// byte-stream reader in that case would race with `ReadConsoleInputW`
/// on the same STDIN console queue. `ReadFile` strips modifier state
/// before producing bytes, so a stolen Shift+Enter surfaces as `\r`
/// instead of `\n`. Returning `false` keeps `console_input` as the
/// sole consumer.
///
/// Every other configuration — POSIX, piped stdin, no `extra_rx` —
/// keeps the byte-stream reader so existing behavior (including
/// `echo "prompt" | clud` and POSIX interactive use) is unchanged.
mod bracketed_paste;
mod child_modes;
mod escape_gate;

pub use bracketed_paste::BracketedPasteNormalizer;
#[cfg(test)]
use bracketed_paste::{PASTE_END, PASTE_START};

mod interrupt;

use interrupt::{interrupt_pty_process, reap_pty_exit};

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;

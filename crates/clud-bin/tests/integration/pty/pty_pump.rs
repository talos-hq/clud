//! Raw PTY pump integration tests for `clud --codex`.
//!
//! Covers `clud::session::run_raw_pty_pump` and its with-resize / with-extra-rx
//! variants: verbatim stdin forwarding (issue #46), voice-mode F3 hooks
//! (issues #13, #41), idle ticks, Ctrl-C / interrupt propagation, resize-channel
//! delivery, prompt exit on child death, raw-mode panic safety, and the
//! Shift+Enter extra_rx round trip (issue #141). Also covers the dedicated
//! stdout-writer-thread decoupling from issue #538 (a slow terminal sink
//! must not delay stdin forwarding) via the `..._for_test` writer-injection
//! seam.
//!
//! Lives separately from `tests/integration/pty/pty_behavior.rs` so each integration-test
//! binary stays under the 1K-LOC ceiling. Shared helpers come from
//! `tests/integration/common/mod.rs`.

use std::io::{Cursor, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use running_process::pty::NativePtyProcess;

use crate::common::{drain_reader, mock_agent_path, through_pty_input, wait_for_mock_ready};

/// Counting hooks for pump integration tests. Records F3 presses,
/// releases, ticks, and can opt into voice interception via `intercept`.
pub(crate) struct CountingHooks {
    intercept: bool,
    f3_presses: std::sync::Arc<std::sync::atomic::AtomicU32>,
    f3_releases: std::sync::Arc<std::sync::atomic::AtomicU32>,
    ticks: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl CountingHooks {
    pub(crate) fn new(intercept: bool) -> Self {
        Self {
            intercept,
            f3_presses: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            f3_releases: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            ticks: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }
}

impl clud::session::InteractiveHooks for CountingHooks {
    fn intercept_f3(&self) -> bool {
        self.intercept
    }
    fn on_f3_press(&mut self, _sink: &mut dyn clud::session::PtyInputSink) -> std::io::Result<()> {
        self.f3_presses
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn on_f3_release(
        &mut self,
        _sink: &mut dyn clud::session::PtyInputSink,
    ) -> std::io::Result<()> {
        self.f3_releases
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn on_tick(&mut self, _sink: &mut dyn clud::session::PtyInputSink) -> std::io::Result<()> {
        self.ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// The pump must forward stdin bytes verbatim — no CSI parsing, no
/// event-loop demultiplexing. This is the regression test for the DSR hang
/// (issue #46): a child TUI that emits `\x1b[6n` and expects the terminal
/// to reply must see our stdin bytes unchanged. We feed arbitrary bytes
/// including DSR queries, escape sequences, and F3 and assert the
/// mock-agent recorded exactly what we sent.
#[test]
fn raw_pump_forwards_stdin_bytes_verbatim() {
    require_pty_or_skip!("raw_pump_forwards_stdin_bytes_verbatim");

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let raw_stdin = tmp.path().join("stdin_raw.bin");
    let ready = tmp.path().join("ready");

    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-read-stdin-ms".to_string(),
        "800".to_string(),
        "--mock-stdin-raw-to".to_string(),
        raw_stdin.to_string_lossy().to_string(),
        "--mock-ready-file".to_string(),
        ready.to_string_lossy().to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");

    // Wait for the child to enter its stdin read loop before we feed.
    // #1310: send only once the child's stdin mode is final.
    wait_for_mock_ready(&process, &ready);

    let payload: &[u8] = b"hello\x1b[6n\x1bOR\x1bOP world\n";
    let interrupted = AtomicBool::new(false);
    let mut hooks = CountingHooks::new(false);

    let _exit = clud::session::run_raw_pty_pump(
        &process,
        &interrupted,
        &mut hooks,
        Cursor::new(payload.to_vec()),
    );

    let _ = process.wait_impl(Some(5.0));
    let _ = drain_reader(&process, Duration::from_millis(300));
    let _ = process.close_impl();

    let got = std::fs::read(&raw_stdin).unwrap_or_default();
    let expected = through_pty_input(payload);
    assert_eq!(
        got, expected,
        "pump must forward stdin bytes verbatim; got {:?}, expected {:?}",
        got, expected
    );
}

/// F3 is observed (not intercepted): each `\x1bOR` in the byte stream
/// fires `on_f3_press` once AND the bytes still reach the child. This is
/// the voice-mode contract: clud reacts to F3 in parallel with forwarding,
/// not instead of forwarding.
#[test]
fn raw_pump_fires_voice_f3_press_while_forwarding_bytes() {
    require_pty_or_skip!("raw_pump_fires_voice_f3_press_while_forwarding_bytes");

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let raw_stdin = tmp.path().join("stdin_raw.bin");
    let ready = tmp.path().join("ready");

    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-read-stdin-ms".to_string(),
        "800".to_string(),
        "--mock-stdin-raw-to".to_string(),
        raw_stdin.to_string_lossy().to_string(),
        "--mock-ready-file".to_string(),
        ready.to_string_lossy().to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");
    // #1310: send only once the child's stdin mode is final.
    wait_for_mock_ready(&process, &ready);

    // Three F3 presses embedded in surrounding text. Trailing `\n` is
    // important: the PTY slave defaults to canonical (line) mode, so the
    // kernel holds input until it sees a newline. Without it, the
    // mock-agent's `stdin.read()` never returns and we'd assert on an
    // empty file. Real usage isn't affected — child TUIs like codex put
    // their own slave into raw mode before reading.
    let payload: &[u8] = b"a\x1bORb\x1bORc\x1bORd\n";
    let interrupted = AtomicBool::new(false);
    let hooks = CountingHooks::new(true); // intercept_f3 == true
    let presses = std::sync::Arc::clone(&hooks.f3_presses);
    let mut hooks = hooks;

    let _exit = clud::session::run_raw_pty_pump(
        &process,
        &interrupted,
        &mut hooks,
        Cursor::new(payload.to_vec()),
    );

    let _ = process.wait_impl(Some(5.0));
    let _ = drain_reader(&process, Duration::from_millis(300));
    let _ = process.close_impl();

    let got = std::fs::read(&raw_stdin).unwrap_or_default();
    let expected = through_pty_input(payload);
    assert_eq!(
        got, expected,
        "F3 interception must NOT eat bytes; child should still see {:?}, got {:?}",
        expected, got
    );
    assert_eq!(
        presses.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "expected 3 F3 presses; got a different count"
    );
}

/// Issue #13 hold-to-record: terminals supporting the kitty keyboard
/// protocol (REPORT_EVENT_TYPES) emit a release event when F3 is let go.
/// The pump must fire `on_f3_release` exactly once per release sequence
/// AND still forward the raw bytes to the child.
#[test]
fn raw_pump_fires_voice_f3_release_when_kitty_sequence_present() {
    require_pty_or_skip!("raw_pump_fires_voice_f3_release_when_kitty_sequence_present");

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let raw_stdin = tmp.path().join("stdin_raw.bin");
    let ready = tmp.path().join("ready");

    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-read-stdin-ms".to_string(),
        "800".to_string(),
        "--mock-stdin-raw-to".to_string(),
        raw_stdin.to_string_lossy().to_string(),
        "--mock-ready-file".to_string(),
        ready.to_string_lossy().to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");
    // #1310: send only once the child's stdin mode is final.
    wait_for_mock_ready(&process, &ready);

    // Kitty F3 press (CSI u, functional encoding) then release. The
    // trailing `\n` is the canonical-mode trigger; without it the
    // mock-agent's stdin read never returns.
    let payload: &[u8] = b"a\x1b[57346;1:1ub\x1b[57346;1:3uc\n";
    let interrupted = AtomicBool::new(false);
    let hooks = CountingHooks::new(true);
    let presses = std::sync::Arc::clone(&hooks.f3_presses);
    let releases = std::sync::Arc::clone(&hooks.f3_releases);
    let mut hooks = hooks;

    let _exit = clud::session::run_raw_pty_pump(
        &process,
        &interrupted,
        &mut hooks,
        Cursor::new(payload.to_vec()),
    );

    let _ = process.wait_impl(Some(5.0));
    let _ = drain_reader(&process, Duration::from_millis(300));
    let _ = process.close_impl();

    let got = std::fs::read(&raw_stdin).unwrap_or_default();
    assert_eq!(
        got,
        through_pty_input(payload),
        "kitty release detection must NOT eat bytes; child should still see the full payload"
    );
    assert_eq!(
        presses.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "expected exactly 1 F3 press; got a different count"
    );
    assert_eq!(
        releases.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "expected exactly 1 F3 release; got a different count"
    );
}

/// `on_tick` must fire on every main-loop iteration regardless of stdin.
/// Voice mode drains a background transcription worker through this hook;
/// gating it behind stdin activity would leave transcripts stuck whenever
/// the user stops typing. Here: an empty stdin source, a 400ms child.
/// We only prove the tick isn't gated on stdin, not measure loop cadence,
/// so a single tick passes. The pump ticks every 50ms (`PUMP_TICK`), and
/// the child has already spent 100ms+ of its 400ms starting up; a slow
/// macOS runner saw only 2 ticks, which failed the old `>= 3` bound.
#[test]
fn raw_pump_calls_on_tick_during_idle() {
    require_pty_or_skip!("raw_pump_calls_on_tick_during_idle");

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let raw_stdin = tmp.path().join("stdin_raw.bin");

    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-read-stdin-ms".to_string(),
        "400".to_string(),
        "--mock-stdin-raw-to".to_string(),
        raw_stdin.to_string_lossy().to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");
    std::thread::sleep(Duration::from_millis(100));

    let interrupted = AtomicBool::new(false);
    let hooks = CountingHooks::new(false);
    let ticks = std::sync::Arc::clone(&hooks.ticks);
    let mut hooks = hooks;

    // Empty stdin: reader thread hits EOF immediately. Main loop then
    // idles and must still tick.
    let _exit = clud::session::run_raw_pty_pump(
        &process,
        &interrupted,
        &mut hooks,
        Cursor::new(Vec::<u8>::new()),
    );

    let _ = process.wait_impl(Some(5.0));
    let _ = drain_reader(&process, Duration::from_millis(300));
    let _ = process.close_impl();

    let tick_count = ticks.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        tick_count >= 1,
        "expected on_tick to fire while stdin was idle, got {}",
        tick_count
    );
}

/// Flipping the shared `interrupted` flag while the pump is running with
/// a long-lived child must cause the pump to return within a couple of
/// seconds. On POSIX this is `send_interrupt_impl` sending SIGINT to
/// the child's pgroup; on Windows it's `close_impl` tearing the PTY
/// down (the Windows `send_interrupt_impl` is intentionally skipped to
/// avoid duplicating the 0x03 byte that stdin already forwarded — see
/// `interrupt_pty_process` for the rationale).
#[test]
fn raw_pump_honors_ctrlc_flag() {
    require_pty_or_skip!("raw_pump_honors_ctrlc_flag");

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let raw_stdin = tmp.path().join("stdin_raw.bin");

    // Long-lived: 5 seconds of blocking stdin read — gives us headroom to
    // observe the interrupt without racing against child exit.
    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-read-stdin-ms".to_string(),
        "5000".to_string(),
        "--mock-stdin-raw-to".to_string(),
        raw_stdin.to_string_lossy().to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");
    std::thread::sleep(Duration::from_millis(150));

    let interrupted = std::sync::Arc::new(AtomicBool::new(false));
    let mut hooks = CountingHooks::new(false);

    // Trip the flag 200ms after the pump starts running.
    let flag = std::sync::Arc::clone(&interrupted);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    let start = Instant::now();
    let _exit = clud::session::run_raw_pty_pump(
        &process,
        &interrupted,
        &mut hooks,
        Cursor::new(Vec::<u8>::new()),
    );
    let elapsed = start.elapsed();

    let _ = process.close_impl();

    assert!(
        elapsed < Duration::from_millis(2500),
        "pump must return within 2.5s of ctrlc flag flip, took {:?}",
        elapsed
    );
}

/// #1704: Ctrl+C kills the child, so a TUI never runs its own exit path and
/// whatever terminal state it set stays set. The pump must feed the child's
/// output to the session's tracker before the interrupt, so the guard's
/// restore turns those modes off afterwards. Drives the real pump, the real
/// interrupt path and a real PTY child that turned modes on and then hangs.
#[test]
fn a_ctrl_c_interrupted_child_has_its_terminal_modes_undone() {
    require_pty_or_skip!("a_ctrl_c_interrupted_child_has_its_terminal_modes_undone");

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let script = tmp.path().join("modes.ansi");
    std::fs::write(
        &script,
        b"\x1b[?1049h\x1b[2;20r\x1b[?1h\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[?25lready",
    )
    .expect("write ansi script");
    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-ansi-script".to_string(),
        script.to_string_lossy().to_string(),
        "--mock-sleep-ms".to_string(),
        "10000".to_string(),
    ];
    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");

    let mut keyboard = clud::session::KeyboardEnhancementGuard::with_pushed(false);
    let interrupted = std::sync::Arc::new(AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&interrupted);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let mut hooks = CountingHooks::new(false);
    let _exit = clud::session::run_raw_pty_pump_with_extras(
        &process,
        &interrupted,
        &mut hooks,
        Cursor::new(Vec::<u8>::new()),
        None,
        false,
        clud::session::PumpExtras {
            keyboard_enhancement_tracker: Some(keyboard.child_tracker()),
            ..clud::session::PumpExtras::default()
        },
    );
    let _ = process.close_impl();

    let mut restore = Vec::new();
    keyboard.unwind_to(&mut restore);
    let has = |seq: &[u8]| restore.windows(seq.len()).any(|w| w == seq);
    // ConPTY re-renders the child's output: it forwards the alternate
    // screen, bracketed paste and cursor visibility, but answers the mouse,
    // scroll-region and cursor-key requests itself (and turns focus
    // reporting on of its own accord), so only POSIX checks the full set.
    let mut expected: Vec<&[u8]> = vec![b"\x1b[?1049l", b"\x1b[?2004l", b"\x1b[?25h"];
    if clud::platform::host::UNIX {
        expected.extend([
            &b"\x1b7\x1b[r\x1b8"[..],
            b"\x1b[?1l",
            b"\x1b[?1003l",
            b"\x1b[?1006l",
            b"\x1b[?1004l",
        ]);
    }
    for seq in expected {
        assert!(
            has(seq),
            "restore after Ctrl+C must contain {seq:?}; got {:?}",
            String::from_utf8_lossy(&restore)
        );
    }
}

/// Resize events delivered through the pump's resize channel must reach
/// `resize_pty` and propagate to the PTY master. This covers both Step 9
/// (Unix SIGWINCH source) and Step 10 (Windows ReadConsoleInputW source)
/// — the OS-specific threads are responsible for *producing* events into
/// this channel, and the pump is responsible for *consuming* them and
/// calling `resize_pty`. Here we bypass the OS source and push directly.
#[test]
fn raw_pump_applies_resize_from_channel() {
    require_pty_or_skip!("raw_pump_applies_resize_from_channel");

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let raw_stdin = tmp.path().join("stdin_raw.bin");

    // Long enough that the resize has time to land before the child exits.
    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-read-stdin-ms".to_string(),
        "600".to_string(),
        "--mock-stdin-raw-to".to_string(),
        raw_stdin.to_string_lossy().to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 20, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");
    std::thread::sleep(Duration::from_millis(100));

    // Sanity: master starts at the size we spawned with.
    {
        let guard = process.handles.lock().expect("handles");
        let handles = guard.as_ref().expect("handles present");
        let size = handles.master.get_size().expect("get_size");
        assert_eq!((size.rows, size.cols), (20, 80));
    }

    let (resize_tx, resize_rx) = std::sync::mpsc::channel::<(u16, u16)>();

    // Push a resize after a short delay so the main loop is already
    // running when it arrives.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        let _ = resize_tx.send((40, 120));
    });

    let interrupted = AtomicBool::new(false);
    let mut hooks = CountingHooks::new(false);

    let _exit = clud::session::run_raw_pty_pump_with_resize_rx(
        &process,
        &interrupted,
        &mut hooks,
        Cursor::new(Vec::<u8>::new()),
        resize_rx,
    );

    // After the pump consumed the resize, the master must reflect it.
    {
        let guard = process.handles.lock().expect("handles");
        let handles = guard.as_ref().expect("handles present");
        let size = handles.master.get_size().expect("get_size");
        assert_eq!(
            (size.rows, size.cols),
            (40, 120),
            "pump did not apply (40,120) resize from channel"
        );
    }

    let _ = process.wait_impl(Some(2.0));
    let _ = process.close_impl();
}

/// When the child exits, the pump must return promptly even if the
/// stdin reader thread is blocked in `read()`. Real `io::stdin()` blocks
/// waiting for the next keystroke — never returning EOF. We simulate
/// that with a `Read` impl that parks forever. The detached reader
/// thread is fine to leave blocked; the main loop's `poll_pty_process`
/// is what drives shutdown.
#[test]
fn raw_pump_exits_promptly_when_child_exits() {
    require_pty_or_skip!("raw_pump_exits_promptly_when_child_exits");

    struct BlockingReader;
    impl std::io::Read for BlockingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            // Mimics `io::stdin()` with no pending input: block indefinitely.
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    }

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let raw_stdin = tmp.path().join("stdin_raw.bin");

    // Short-lived child — it reads stdin for 400ms then exits.
    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-read-stdin-ms".to_string(),
        "400".to_string(),
        "--mock-stdin-raw-to".to_string(),
        raw_stdin.to_string_lossy().to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");

    let interrupted = AtomicBool::new(false);
    let mut hooks = CountingHooks::new(false);

    let start = Instant::now();
    let _exit = clud::session::run_raw_pty_pump(&process, &interrupted, &mut hooks, BlockingReader);
    let elapsed = start.elapsed();

    let _ = process.close_impl();

    assert!(
        elapsed < Duration::from_millis(1500),
        "pump must exit within 1.5s after child dies, took {:?}",
        elapsed
    );
}
/// fire and disable raw mode — otherwise the user's terminal is left in
/// a broken state after a crash. `catch_unwind` wraps the panicking hook
/// to assert recovery.
#[test]
fn raw_pump_restores_raw_mode_on_panic() {
    require_pty_or_skip!("raw_pump_restores_raw_mode_on_panic");

    // Note: the pump itself doesn't own a RawTerminalGuard — `run_plan_pty`
    // in main.rs does (will, once wired in Step 13). Raw mode is a
    // caller-side concern. This test verifies the pump doesn't leak raw
    // mode *on panic*: if we enter raw mode before calling the pump with
    // a panicking hook, the guard on the main stack frame unwinds and
    // restores the terminal.
    struct PanickingHooks;
    impl clud::session::InteractiveHooks for PanickingHooks {
        fn on_tick(&mut self, _sink: &mut dyn clud::session::PtyInputSink) -> std::io::Result<()> {
            panic!("deliberate panic in on_tick");
        }
    }

    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let raw_stdin = tmp.path().join("stdin_raw.bin");

    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-read-stdin-ms".to_string(),
        "500".to_string(),
        "--mock-stdin-raw-to".to_string(),
        raw_stdin.to_string_lossy().to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");
    std::thread::sleep(Duration::from_millis(100));

    let interrupted = AtomicBool::new(false);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut hooks = PanickingHooks;
        clud::session::run_raw_pty_pump(
            &process,
            &interrupted,
            &mut hooks,
            Cursor::new(Vec::<u8>::new()),
        )
    }));

    assert!(result.is_err(), "hook panic must propagate to catch_unwind");

    // The crossterm raw-mode state is a caller concern; we just verify the
    // pump returned control (panic surfaced) rather than hung after the
    // panic unwound through its own threads.
    let _ = process.wait_impl(Some(2.0));
    let _ = process.close_impl();
}

/// Regression: a Ctrl-C byte (0x03) arriving via `extra_rx` must trigger
/// the pump's interrupt path. This is the path the Windows
/// `console_input` reader uses when `ENABLE_PROCESSED_INPUT` is off and
/// the OS no longer fires `CTRL_C_EVENT` — without this check clud
/// would forward the byte to the child but never observe Ctrl-C itself,
/// leaving users unable to exit (interactive PTY mode regression from
/// PR #144 / commit a3ec8fe).
///
/// The test does NOT require Windows: it exercises the pump-level
/// invariant on every platform, which is exactly what regressed.
#[test]
fn extra_rx_ctrl_c_byte_interrupts_pump() {
    require_pty_or_skip!("extra_rx_ctrl_c_byte_interrupts_pump");

    let agent = mock_agent_path();
    // Keep the child alive longer than the test deadline so a returning
    // pump must be the result of the interrupt path, not a natural exit.
    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-sleep-ms".to_string(),
        "10000".to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");
    std::thread::sleep(Duration::from_millis(150));

    let (extra_tx, extra_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        let _ = extra_tx.send(vec![0x03]);
    });

    let interrupted = AtomicBool::new(false);
    let mut hooks = CountingHooks::new(false);

    let start = Instant::now();
    let exit = clud::session::run_raw_pty_pump_with_extra_rx(
        &process,
        &interrupted,
        &mut hooks,
        Cursor::new(Vec::<u8>::new()),
        Some(extra_rx),
    );
    let elapsed = start.elapsed();

    let _ = process.close_impl();

    assert!(
        elapsed < Duration::from_secs(5),
        "pump should return promptly after extra_rx Ctrl-C; took {:?}",
        elapsed
    );
    // On Windows the contract is the clean 130 returned by
    // `interrupt_pty_process` after `close_impl`. On POSIX the value
    // is whatever the SIGINT-killed child reports through the PTY
    // wait path, which is currently unreliable for an unrelated
    // running-process 4.0.x regression (see zackees/clud#159). The
    // load-bearing invariant for this fix is that the pump returned
    // at all — without the fix it would block on the 10s child sleep.
    if clud::platform::host::WINDOWS {
        assert_eq!(
            exit, 130,
            "extra_rx Ctrl-C must yield SIGINT-convention exit code 130 on Windows; got {}",
            exit
        );
    }
}

/// Regression for issue #1101: Ctrl+C in the kitty keyboard protocol's
/// CSI u encoding must interrupt the pump the same way the legacy `0x03`
/// byte does.
///
/// clud used to push `DISAMBIGUATE_ESCAPE_CODES` alongside
/// `REPORT_EVENT_TYPES`, which makes a kitty-protocol terminal send Ctrl+C
/// as `\x1b[99;5u` instead of `0x03`. The pump's only interrupt detector was
/// `chunk.contains(&0x03)`, and raw mode had already cleared `ISIG` so the
/// `ctrlc` SIGINT handler could not cover for it either — a `clud grind` run
/// could only be stopped by closing the terminal window, while the forwarded
/// sequences echoed back from the child as literal `^[[99;5:2u` text.
///
/// The primary fix is not pushing that flag (see `KEYBOARD_ENHANCEMENT_FLAGS`
/// in `session.rs`). This test pins the backstop: whoever pushes the flag —
/// a child TUI, or a future change here — Ctrl+C still has to work. It uses
/// the autorepeat spelling (`:2u`) because that is what a *held* Ctrl+C
/// emits, which is the state the reporter was actually stuck in.
///
/// Mirrors `extra_rx_ctrl_c_byte_interrupts_pump`, including its POSIX exit
/// code caveat: the load-bearing assertion is that the pump returned at all
/// rather than blocking on the child's 10s sleep.
#[test]
fn extra_rx_kitty_csi_u_ctrl_c_interrupts_pump() {
    require_pty_or_skip!("extra_rx_kitty_csi_u_ctrl_c_interrupts_pump");

    let agent = mock_agent_path();
    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-sleep-ms".to_string(),
        "10000".to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");
    std::thread::sleep(Duration::from_millis(150));

    let (extra_tx, extra_rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        let _ = extra_tx.send(b"\x1b[99;5:2u".to_vec());
    });

    let interrupted = AtomicBool::new(false);
    let mut hooks = CountingHooks::new(false);

    let start = Instant::now();
    let exit = clud::session::run_raw_pty_pump_with_extra_rx(
        &process,
        &interrupted,
        &mut hooks,
        Cursor::new(Vec::<u8>::new()),
        Some(extra_rx),
    );
    let elapsed = start.elapsed();

    let _ = process.close_impl();

    assert!(
        elapsed < Duration::from_secs(5),
        "pump should return promptly after a CSI u Ctrl+C; took {:?}",
        elapsed
    );
    if clud::platform::host::WINDOWS {
        assert_eq!(
            exit, 130,
            "CSI u Ctrl+C must yield SIGINT-convention exit code 130 on Windows; got {}",
            exit
        );
    }
}

// ─── Issue #538: dedicated writer thread decouples stdin forwarding from a
// stalled stdout sink ────────────────────────────────────────────────────

/// `Read` source fed by an `mpsc::Sender<Vec<u8>>`, so a test can push
/// stdin bytes into the pump's byte-stream reader thread at a precise
/// moment (unlike `Cursor`, which hands over all its bytes immediately and
/// then EOFs). Blocks in `read()` until a chunk arrives or the sender is
/// dropped (EOF). Owns its `Receiver`, so it satisfies the pump's
/// `R: Read + Send + 'static` bound.
struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl ChannelReader {
    fn new(rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            pending: Vec::new(),
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.is_empty() {
            match self.rx.recv() {
                Ok(chunk) => self.pending = chunk,
                Err(_) => return Ok(0), // sender dropped -> EOF
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

/// A `Write` sink whose `flush()` blocks for a fixed duration — standing in
/// for a terminal that's slow to accept output (the issue #538 repro case:
/// high CPU load). Counters are `Arc`-backed so a clone kept by the test
/// can observe writer-thread activity after the sink itself has been moved
/// into the pump.
#[derive(Clone)]
struct SlowSink {
    sleep: Duration,
    flush_calls: std::sync::Arc<AtomicUsize>,
}

impl SlowSink {
    fn new(sleep: Duration) -> Self {
        Self {
            sleep,
            flush_calls: std::sync::Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Write for SlowSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.sleep);
        Ok(())
    }
}

/// Acceptance criterion (issue #538): "Keystroke-to-PTY forwarding no
/// longer waits on terminal `flush()` (verified by a test with an
/// artificially slow stdout sink: input forwarding latency stays low while
/// output stalls)."
///
/// Drives the real pump (`run_raw_pty_pump_full_with_writer_for_test`)
/// against a live PTY child that flushes one sizeable output burst at
/// startup, with a `SlowSink` standing in for the terminal. Once the
/// dedicated writer thread has actually entered its slow `flush()` (polled
/// via `flush_calls`, not a fixed sleep guess — this is what proves the
/// premise instead of assuming it), a stdin byte is pushed and timed via
/// `NativePtyProcess::pty_input_bytes_total()`, a counter `write_impl`
/// bumps synchronously and independently of whether the child ever reads
/// it. Pre-#538, output read → filter → write → flush ran inline ahead of
/// stdin draining in the same loop turn, so a slow `flush()` would have
/// delayed this measurement by roughly `SINK_SLEEP`; the dedicated writer
/// thread (fed over an unbounded channel that never blocks `send`)
/// decouples them.
#[test]
fn stdin_forwarding_stays_fast_while_output_sink_stalls() {
    require_pty_or_skip!("stdin_forwarding_stays_fast_while_output_sink_stalls");

    // The gap between these two is the test's discriminating power:
    // pre-#538 the inline `flush()` put a full SINK_SLEEP (or more) in
    // front of stdin forwarding, so even a heavily loaded CI runner only
    // clears MAX_FORWARD_LATENCY when the decoupling actually works.
    const SINK_SLEEP: Duration = Duration::from_millis(500);
    const MAX_FORWARD_LATENCY: Duration = Duration::from_millis(200);

    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let ansi_path = tmp.path().join("burst.txt");
    // One sizeable, plain-ASCII burst the child flushes immediately at
    // startup (before it ever touches stdin) — enough to occupy the slow
    // sink's flush() for the full SINK_SLEEP once the pump's reader/writer
    // threads pick it up. Plain ASCII so the OSC title stripper is a no-op.
    std::fs::write(&ansi_path, "x".repeat(8000)).expect("write ansi burst");

    let agent = mock_agent_path();
    let argv = vec![
        agent.to_string_lossy().to_string(),
        "--mock-ansi-script".to_string(),
        ansi_path.to_string_lossy().to_string(),
        // Keeps the child alive well past our measurement window without
        // touching stdin at all, so it can never race the test's own
        // stdin push.
        "--mock-sleep-ms".to_string(),
        "3000".to_string(),
    ];

    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");
    // Let the child start and flush its burst into the PTY's read queue.
    std::thread::sleep(Duration::from_millis(150));

    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
    let stdin_source = ChannelReader::new(stdin_rx);
    let (_resize_tx, resize_rx) = mpsc::channel::<(u16, u16)>();
    let interrupted = AtomicBool::new(false);
    let mut hooks = CountingHooks::new(false);
    let sink = SlowSink::new(SINK_SLEEP);
    let sink_stats = sink.clone();

    std::thread::scope(|scope| {
        let pump = scope.spawn(|| {
            clud::session::run_raw_pty_pump_full_with_writer_for_test(
                &process,
                &interrupted,
                &mut hooks,
                stdin_source,
                resize_rx,
                None,
                sink,
            )
        });

        // Don't guess when the writer thread is mid-flush — poll for it.
        // This is the test's proof that the premise ("output is genuinely
        // stalled right now") actually holds at measurement time.
        let stall_deadline = Instant::now() + Duration::from_secs(5);
        while sink_stats.flush_calls.load(Ordering::SeqCst) == 0 && Instant::now() < stall_deadline
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            sink_stats.flush_calls.load(Ordering::SeqCst) >= 1,
            "test premise unmet: writer thread never entered flush() on the slow sink \
             within 5s, so it never actually stalled"
        );

        let before = process.pty_input_bytes_total();
        let start = Instant::now();
        stdin_tx.send(b"x".to_vec()).expect("send stdin byte");

        let forward_deadline = Instant::now() + Duration::from_secs(1);
        let mut after = before;
        while Instant::now() < forward_deadline {
            after = process.pty_input_bytes_total();
            if after > before {
                break;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        let elapsed = start.elapsed();

        // Tear down: flip the interrupt flag so the pump (and its reader
        // + writer threads) unwind, then join before asserting so a
        // failure doesn't leak the scoped thread.
        interrupted.store(true, Ordering::SeqCst);
        drop(stdin_tx);
        let _ = pump.join().expect("pump thread panicked");

        assert!(
            after > before,
            "stdin byte never reached pty_input_bytes_total within 1s while the output \
             sink was stalled (before={before}, after={after}); forwarding may be blocked \
             behind the writer thread"
        );
        assert!(
            elapsed < MAX_FORWARD_LATENCY,
            "stdin forwarding took {:?} while the output sink was stalled in a {:?} \
             flush() — expected forwarding to be decoupled from the writer thread \
             (issue #538), i.e. well under the sink's stall duration",
            elapsed,
            SINK_SLEEP
        );
    });

    let _ = process.wait_impl(Some(2.0));
    let _ = process.close_impl();
}

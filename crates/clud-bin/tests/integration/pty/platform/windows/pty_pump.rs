//! The Windows-only half of `pty/pty_pump.rs`: `extra_rx` round trips that
//! drive clud's real native terminal-input adapter (`TerminalInputCore`) and
//! ConPTY into mock-agent. Shared helpers come from `pty/pty_pump.rs` and
//! `tests/integration/common/mod.rs`.

use std::io::Cursor;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use running_process::pty::NativePtyProcess;

use crate::common::{drain_reader, mock_agent_path, wait_for_mock_ready};
use crate::pty::pty_pump::CountingHooks;

/// Issues #141/#575: translated events from the native console-input adapter
/// reach the child PTY unchanged. The pump doesn't know or care where the
/// bytes came from — it forwards `extra_rx` chunks to the child the same way
/// it would forward stdin. This integration test asserts that invariant by:
///
///   1. Injecting upstream terminal events into clud's real adapter.
///   2. Sending the adapter receiver through the production PTY pump.
///   3. Capturing the child's stdin via `--mock-stdin-raw-to`.
///   4. Asserting complete arrow CSI chunks plus Shift+Enter ESC CR (#1369)
///      and plain Enter `\r` made the round trip.
///
/// The separate `shift_enter_dual_reader` test keeps the upstream translation
/// and trace assertions active on headless Windows. This child-PTY half uses
/// the standard canary because ConPTY cannot reliably run when the parent
/// process has redirected output.
#[test]
fn extra_rx_forwards_native_terminal_adapter_bytes_to_pty() {
    require_pty_or_skip!("extra_rx_forwards_native_terminal_adapter_bytes_to_pty");

    use running_process::pty::terminal_input::{TerminalInputCore, TerminalInputEventRecord};

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

    let core = std::sync::Arc::new(TerminalInputCore::new());
    {
        let mut state = core.state.lock().expect("terminal input state");
        state.closed = false;
    }
    let mut input = clud::console_input::spawn_terminal_input_adapter(std::sync::Arc::clone(&core))
        .expect("spawn terminal input adapter");
    let extra_rx = input.take_receiver().expect("terminal input receiver");

    let event = |data: &[u8], virtual_key_code: u16, shift: bool| TerminalInputEventRecord {
        data: data.to_vec(),
        submit: virtual_key_code == 0x0D && !shift,
        shift,
        ctrl: false,
        alt: false,
        virtual_key_code,
        repeat_count: 1,
    };
    {
        let mut state = core.state.lock().expect("terminal input state");
        state.events.extend([
            event(b"\x1b[D", 0x25, false),
            event(b"\x1b[B", 0x28, false),
            event(b"\x1b[C", 0x27, false),
            event(b"\x1b[A", 0x26, false),
            event(b"\x1b[13;2u", 0x0D, true),
            event(b"\r", 0x0D, false),
        ]);
    }
    core.condvar.notify_all();

    let interrupted = AtomicBool::new(false);
    let mut hooks = CountingHooks::new(false);

    let _exit = clud::session::run_raw_pty_pump_with_extra_rx(
        &process,
        &interrupted,
        &mut hooks,
        Cursor::new(Vec::<u8>::new()),
        Some(extra_rx),
    );

    let _ = process.wait_impl(Some(5.0));
    let _ = drain_reader(&process, Duration::from_millis(300));
    let _ = process.close_impl();

    let got = std::fs::read(&raw_stdin).unwrap_or_default();
    assert!(
        got.windows(b"\x1b[D\x1b[B\x1b[C\x1b[A".len())
            .any(|window| window == b"\x1b[D\x1b[B\x1b[C\x1b[A"),
        "complete navigation sequences must reach the child PTY; got {got:?}"
    );
    // Shift+Enter is sent as ESC CR, which survives ConPTY unchanged and
    // stays distinct from plain Enter (#1369).
    assert!(
        got.windows(2).any(|window| window == b"\x1b\r"),
        "Shift+Enter translation must reach the child as ESC CR; got {got:?}"
    );
    assert!(
        got.contains(&b'\r'),
        "plain Enter translation must produce a \\r in the child's stdin; got {:?}",
        got
    );
}

/// #1697: under VT input the Windows console delivers a terminal's SGR mouse
/// report as one `KEY_EVENT` per character, so the adapter emits it one byte
/// per event. ConPTY flushes a sequence left open at the end of each write,
/// so if the pump wrote those bytes separately the child would lose `ESC [ <`
/// and read `35;31;18M` as typed text. Drives the real adapter, pump and
/// ConPTY into mock-agent, which reads with VT input like Claude Code.
#[test]
fn extra_rx_mouse_reports_split_per_character_reach_the_child_whole() {
    require_pty_or_skip!("extra_rx_mouse_reports_split_per_character_reach_the_child_whole");

    use running_process::pty::terminal_input::{TerminalInputCore, TerminalInputEventRecord};

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
    wait_for_mock_ready(&process, &ready);

    let core = std::sync::Arc::new(TerminalInputCore::new());
    {
        let mut state = core.state.lock().expect("terminal input state");
        state.closed = false;
    }
    let mut input = clud::console_input::spawn_terminal_input_adapter(std::sync::Arc::clone(&core))
        .expect("spawn terminal input adapter");
    let extra_rx = input.take_receiver().expect("terminal input receiver");

    let reports: &[&[u8]] = &[b"\x1b[<35;31;18M", b"\x1b[<35;31;19M", b"\x1b[<35;33;20M"];
    let char_event = |byte: u8| TerminalInputEventRecord {
        data: vec![byte],
        submit: false,
        shift: false,
        ctrl: false,
        alt: false,
        virtual_key_code: 0,
        repeat_count: 1,
    };
    {
        let mut state = core.state.lock().expect("terminal input state");
        state
            .events
            .extend(reports.concat().into_iter().map(char_event));
    }
    core.condvar.notify_all();

    let interrupted = AtomicBool::new(false);
    let mut hooks = CountingHooks::new(false);
    let _exit = clud::session::run_raw_pty_pump_with_extra_rx(
        &process,
        &interrupted,
        &mut hooks,
        Cursor::new(Vec::<u8>::new()),
        Some(extra_rx),
    );

    let _ = process.wait_impl(Some(5.0));
    let _ = drain_reader(&process, Duration::from_millis(300));
    let _ = process.close_impl();

    let got = std::fs::read(&raw_stdin).unwrap_or_default();
    let expected = reports.concat();
    assert!(
        got.windows(expected.len()).any(|window| window == expected),
        "the child must read whole SGR mouse reports, not `ESC [ <`-less text; got {:?}",
        String::from_utf8_lossy(&got)
    );
}

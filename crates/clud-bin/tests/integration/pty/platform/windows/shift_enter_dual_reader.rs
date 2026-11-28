//! Production-path Windows terminal-input regression test.
//!
//! The test injects real `KEY_EVENT_RECORD`s into the process console with
//! `WriteConsoleInputW`, then observes the bytes emitted by clud's
//! `TerminalInputCore` adapter. It covers the navigation-key failure from
//! issue #575, the Shift+Enter compatibility behavior from issue #141, and
//! surrogate-pair (emoji) assembly from issue #1351.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::time::Duration;

use clud::console_input::spawn_console_input_reader;
use running_process::pty::terminal_input::translate_console_key_event;
use winapi::um::wincontypes::KEY_EVENT_RECORD as WinapiKeyEventRecord;
use windows::Win32::System::Console::{
    GetStdHandle, WriteConsoleInputW, INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT, KEY_EVENT_RECORD,
    KEY_EVENT_RECORD_0, STD_INPUT_HANDLE,
};
use windows_core::BOOL;

const VK_RETURN: u16 = 0x0D;
/// Virtual-key code the console reports for synthesized Unicode text.
const VK_PACKET: u16 = 0xE7;
const SHIFT_PRESSED: u32 = 0x0010;
const TRACE_ENV: &str = "RUNNING_PROCESS_NATIVE_TERMINAL_INPUT_TRACE_PATH";

struct EnvRestore {
    previous: Option<OsString>,
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        if let Some(value) = self.previous.take() {
            std::env::set_var(TRACE_ENV, value);
        } else {
            std::env::remove_var(TRACE_ENV);
        }
    }
}

fn key_record(key_down: bool, virtual_key: u16, unicode: u16, control: u32) -> INPUT_RECORD {
    INPUT_RECORD {
        EventType: KEY_EVENT as u16,
        Event: INPUT_RECORD_0 {
            KeyEvent: KEY_EVENT_RECORD {
                bKeyDown: BOOL(if key_down { 1 } else { 0 }),
                wRepeatCount: 1,
                wVirtualKeyCode: virtual_key,
                wVirtualScanCode: 0,
                uChar: KEY_EVENT_RECORD_0 {
                    UnicodeChar: unicode,
                },
                dwControlKeyState: control,
            },
        },
    }
}

fn inject_records(records: &[INPUT_RECORD]) {
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.expect("GetStdHandle");
    let mut written = 0;
    unsafe { WriteConsoleInputW(handle, records, &mut written) }.expect("WriteConsoleInputW");
    assert_eq!(
        written as usize,
        records.len(),
        "WriteConsoleInputW must write every record"
    );
}

fn inject_key(virtual_key: u16, unicode: u16, control: u32) {
    inject_records(&[
        key_record(true, virtual_key, unicode, control),
        key_record(false, virtual_key, unicode, control),
    ]);
}

fn upstream_key_record(virtual_key: u16, unicode: u16, control: u32) -> WinapiKeyEventRecord {
    // SAFETY: zero is a valid baseline for KEY_EVENT_RECORD and its union. We
    // initialize every field consumed by running-process before use.
    let mut record: WinapiKeyEventRecord = unsafe { std::mem::zeroed() };
    record.bKeyDown = 1;
    record.wRepeatCount = 1;
    record.wVirtualKeyCode = virtual_key;
    record.dwControlKeyState = control;
    // SAFETY: UnicodeChar is the union arm consumed by
    // `translate_console_key_event`.
    unsafe {
        *record.uChar.UnicodeChar_mut() = unicode;
    }
    record
}

#[test]
fn native_reader_translates_navigation_and_preserves_shift_enter() {
    let temp = kernal_api::platform::fs::TemporaryDirectory::new().expect("trace tempdir");
    let trace_path = temp.path().join("native-input.trace");
    let _env_restore = EnvRestore {
        previous: std::env::var_os(TRACE_ENV),
    };
    std::env::set_var(TRACE_ENV, &trace_path);

    const NAVIGATION_CASES: &[(u16, &[u8])] = &[
        (0x25, b"\x1b[D"),  // Left
        (0x28, b"\x1b[B"),  // Down
        (0x27, b"\x1b[C"),  // Right
        (0x26, b"\x1b[A"),  // Up
        (0x24, b"\x1b[H"),  // Home
        (0x23, b"\x1b[F"),  // End
        (0x2D, b"\x1b[2~"), // Insert
        (0x2E, b"\x1b[3~"), // Delete
        (0x21, b"\x1b[5~"), // Page Up
        (0x22, b"\x1b[6~"), // Page Down
    ];

    // Always exercise the exact running-process translator clud consumes,
    // including its trace output. This remains meaningful in headless test
    // runners where STDIN is not an attached console.
    for &(virtual_key, expected) in NAVIGATION_CASES {
        let record = upstream_key_record(virtual_key, 0, 0);
        let translated = translate_console_key_event(&record).expect("translated navigation event");
        assert_eq!(
            translated.data, expected,
            "upstream virtual-key code {virtual_key:#x}"
        );
    }

    if !std::io::stdin().is_terminal() {
        eprintln!(
            "native_reader_translates_navigation_and_preserves_shift_enter: \
             production console capture SKIP (stdin not a real console)"
        );
    } else {
        let mut input = spawn_console_input_reader().expect("spawn native terminal reader");
        let rx = input.take_receiver().expect("terminal input receiver");

        // Each key event must arrive as one complete chunk. In particular, the
        // leading Escape byte may not be split from or dropped before the CSI
        // suffix reaches Codex.
        for &(virtual_key, expected) in NAVIGATION_CASES {
            inject_key(virtual_key, 0, 0);
            let got = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("translated navigation event");
            assert_eq!(got, expected, "virtual-key code {virtual_key:#x}");
        }

        // running-process emits CSI-u for Shift+Enter; clud sends ESC CR at
        // the adapter boundary because ConPTY rewrites a bare LF into CR (#1369).
        inject_key(VK_RETURN, b'\r' as u16, SHIFT_PRESSED);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2))
                .expect("Shift+Enter event"),
            b"\x1b\r"
        );

        inject_key(VK_RETURN, b'\r' as u16, 0);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2))
                .expect("plain Enter event"),
            b"\r"
        );

        // #1351: an emoji arrives as one down/up record pair per UTF-16
        // surrogate, the way the emoji picker and keystroke paste send it.
        let emoji: Vec<INPUT_RECORD> = "\u{1F600}"
            .encode_utf16()
            .flat_map(|unit| {
                [
                    key_record(true, VK_PACKET, unit, 0),
                    key_record(false, VK_PACKET, unit, 0),
                ]
            })
            .collect();
        inject_records(&emoji);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2))
                .expect("emoji event"),
            "\u{1F600}".as_bytes()
        );

        drop(input);
    }

    let trace = std::fs::read_to_string(&trace_path).expect("native input trace");
    for expected_hex in ["[1b 5b 44]", "[1b 5b 42]", "[1b 5b 43]", "[1b 5b 41]"] {
        assert!(
            trace.contains(&format!("translated bytes={expected_hex}")),
            "trace did not contain {expected_hex}: {trace}"
        );
    }
}

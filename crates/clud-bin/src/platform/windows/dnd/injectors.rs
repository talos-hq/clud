//! Windows subprocess-mode drop injection for [`crate::dnd::injectors`]:
//! the dropped paths are typed into the console input buffer with
//! `WriteConsoleInputW` (#1370). Moved here from `dnd/injectors.rs`
//! (#1953); the pure record encoding stays in that neutral module.

use crate::dnd::console_drop_target::DropInjector;
use crate::dnd::injectors::{
    build_input_records, check_all_records_written, input_record_count, join_paths_for_injection,
    INPUT_RECORD_SIZE,
};
#[cfg(test)]
use crate::dnd::injectors::{KEY_EVENT, VK_RETURN};

/// Build a subprocess-mode injector that writes the joined paths into
/// the *console input buffer* via `WriteConsoleInputW`. The backend's
/// `ReadFile`/`ReadConsole` then sees them as if typed.
pub fn subprocess_console_injector() -> DropInjector {
    console_input_injector(std_input_handle)
}

/// A subprocess-mode injector writing to whatever console input handle
/// `resolve` returns at drop time. [`subprocess_console_injector`] passes
/// the process's standard input; tests pass a `CONIN$` handle.
pub fn console_input_injector(
    resolve: fn() -> std::io::Result<windows::Win32::Foundation::HANDLE>,
) -> DropInjector {
    Box::new(move |paths: &[String]| {
        if paths.is_empty() {
            return;
        }
        let payload = join_paths_for_injection(paths);
        let records = build_input_records(&payload);
        // Best-effort — like the PTY path, we can't surface failures
        // from inside the OLE callback.
        if let Ok(handle) = resolve() {
            let _ = write_records_to_handle(handle, &records);
        }
    })
}

fn std_input_handle() -> std::io::Result<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
    // SAFETY: no preconditions.
    unsafe { GetStdHandle(STD_INPUT_HANDLE) }
        .map_err(|e| std::io::Error::other(format!("GetStdHandle(STD_INPUT_HANDLE) failed: {e}")))
}

/// Windows-only: write a pre-built `INPUT_RECORD` byte buffer to the
/// console input buffer attached to the current process.
///
/// `records_bytes` must be a multiple of [`INPUT_RECORD_SIZE`]; partial
/// records are an error.
pub fn write_to_console_input(records_bytes: &[u8]) -> std::io::Result<()> {
    if records_bytes.is_empty() {
        return Ok(());
    }
    input_record_count(records_bytes.len())?;
    write_records_to_handle(std_input_handle()?, records_bytes)
}

/// Decode the wire bytes from [`build_input_records`] into real
/// `INPUT_RECORD`s, field by field.
///
/// The bytes are never reinterpreted in place: `INPUT_RECORD` needs
/// 4-byte alignment and a `&[u8]` guarantees 1, so casting the pointer
/// would be undefined behavior for any buffer that does not happen to
/// start on a 4-byte boundary (#1370).
pub fn decode_input_records(
    records_bytes: &[u8],
) -> std::io::Result<Vec<windows::Win32::System::Console::INPUT_RECORD>> {
    use windows::Win32::System::Console::{
        INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0,
    };
    input_record_count(records_bytes.len())?;
    let u16_at = |r: &[u8], at: usize| u16::from_le_bytes([r[at], r[at + 1]]);
    let u32_at = |r: &[u8], at: usize| u32::from_le_bytes([r[at], r[at + 1], r[at + 2], r[at + 3]]);
    Ok(records_bytes
        .as_chunks::<INPUT_RECORD_SIZE>()
        .0
        .iter()
        .map(|r| INPUT_RECORD {
            EventType: u16_at(r, 0),
            Event: INPUT_RECORD_0 {
                KeyEvent: KEY_EVENT_RECORD {
                    bKeyDown: (u32_at(r, 4) != 0).into(),
                    wRepeatCount: u16_at(r, 8),
                    wVirtualKeyCode: u16_at(r, 10),
                    wVirtualScanCode: u16_at(r, 12),
                    uChar: KEY_EVENT_RECORD_0 {
                        UnicodeChar: u16_at(r, 14),
                    },
                    dwControlKeyState: u32_at(r, 16),
                },
            },
        })
        .collect())
}

/// Windows-only: write `records_bytes` to the console input `handle`,
/// failing on a short write.
pub fn write_records_to_handle(
    handle: windows::Win32::Foundation::HANDLE,
    records_bytes: &[u8],
) -> std::io::Result<()> {
    use windows::Win32::System::Console::WriteConsoleInputW;

    let records = decode_input_records(records_bytes)?;
    if records.is_empty() {
        return Ok(());
    }
    let mut written: u32 = 0;
    // SAFETY: `records` is a live, properly aligned slice and `written`
    // outlives the call.
    unsafe { WriteConsoleInputW(handle, &records, &mut written) }
        .map_err(|e| std::io::Error::other(format!("WriteConsoleInputW failed: {e}")))?;
    check_all_records_written(records.len(), written as usize)
}

/// Windows tests against a real console input buffer (#1370).
#[cfg(test)]
#[path = "injectors_win_tests.rs"]
pub(crate) mod win_tests;

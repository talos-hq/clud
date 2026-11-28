//! UTF-8 codepage round-trip for the Windows cmd.exe wrapper (issue #168).
//!
//! Real bug being pinned: on Windows the cmd.exe shim that wraps a `.cmd` /
//! `.bat` agent runs under the user's OEM/ANSI codepage (CP437 in US-EN,
//! CP1252 in WEU, CP932 / CP949 / CP936 / CP1251 elsewhere). Anything the
//! shim's tree writes that isn't pure ASCII gets mojibaked on the way to
//! clud's stdout capture. PR for this issue prepends `chcp 65001 > nul &`
//! to the rendered command so the whole shim subtree runs under UTF-8.
//!
//! This test pins the codepage behavior end-to-end by:
//!
//! 1. Writing a UTF-8 byte string (CJK + emoji + accented Latin) to a tempfile.
//! 2. Authoring a one-line `.cmd` file that `type`s that tempfile to stdout.
//! 3. Routing the launch through `subprocess::command_spec_for_subprocess`,
//!    which is the single decision point that owns the cmd.exe wrap.
//! 4. Spawning the resulting `CommandSpec::Shell` via `NativeProcess`
//!    with capture on, draining stdout, and asserting the captured
//!    bytes equal the original UTF-8 payload (modulo a trailing CRLF
//!    `type` may append).
//!
//! Windows-only because the cmd.exe wrap path is `#[cfg(windows)]` in
//! `subprocess.rs`; POSIX never reaches it.

use std::time::Duration;

use clud::subprocess::command_spec_for_subprocess;
use running_process::{NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode};

/// The codepage prefix should let a `.cmd` shim emit non-ASCII UTF-8
/// bytes that survive the round trip back through clud's capture.
#[test]
fn cmd_wrapper_round_trips_utf8_bytes() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");

    // Mix of CJK, accented Latin, emoji — bytes that would mojibake under
    // every common non-UTF-8 Windows codepage.
    let payload: &[u8] = "你好 — café — 🦀".as_bytes();
    let payload_path = tmp.path().join("payload.txt");
    std::fs::write(&payload_path, payload).expect("write payload");

    // `@type` echoes the file's bytes verbatim; `@echo off` keeps the
    // shim from printing the command itself. The path is quoted so spaces
    // in the tempdir don't break parsing.
    let shim_path = tmp.path().join("emit.cmd");
    let shim_body = format!("@echo off\r\n@type \"{}\"\r\n", payload_path.display());
    std::fs::write(&shim_path, shim_body).expect("write shim");

    // This is the seam under test: command_spec_for_subprocess is what
    // injects the chcp 65001 prefix on Windows .cmd / .bat invocations.
    let command = command_spec_for_subprocess(vec![shim_path.to_string_lossy().into_owned()]);
    match &command {
        running_process::CommandSpec::Shell(s) => {
            assert!(
                s.starts_with("chcp 65001 > nul & "),
                "expected UTF-8 codepage prefix; got: {s}"
            );
        }
        other => panic!("expected Shell variant for .cmd shim, got {other:?}"),
    }

    let process = NativeProcess::new(ProcessConfig {
        command,
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().expect("start cmd shim");

    let mut buf = Vec::<u8>::new();
    loop {
        match process.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(event) => {
                buf.extend_from_slice(&event.line);
                buf.push(b'\n');
            }
            ReadStatus::Timeout => {
                if process.returncode().is_some() {
                    break;
                }
            }
            ReadStatus::Eof => break,
        }
    }
    let exit = process
        .wait(Some(Duration::from_secs(10)))
        .expect("wait cmd shim");
    // `wait` ran the bounded capture drain; keep lines that trailed the exit (#2159).
    for event in process.drain_combined() {
        buf.extend_from_slice(&event.line);
        buf.push(b'\n');
    }
    assert_eq!(exit, 0, "cmd shim exited non-zero: {exit}");

    // `type` may emit a trailing CRLF that ReadStatus::Line replaces with
    // `\n`; trim trailing whitespace before comparison.
    let captured = String::from_utf8_lossy(&buf).into_owned();
    let trimmed = captured.trim_end_matches(['\r', '\n']);
    let expected = std::str::from_utf8(payload).expect("payload is utf-8");
    assert_eq!(
        trimmed,
        expected,
        "UTF-8 bytes mojibaked through the cmd.exe wrapper. \
         captured={trimmed:?} (len={}, raw_bytes={:?}); expected={expected:?}",
        trimmed.len(),
        buf
    );
}

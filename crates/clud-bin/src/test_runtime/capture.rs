//! Bounded parent-owned capture for the local test-report tool.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use running_process::{spawn_argv_with_stdio, SpawnStdio, SpawnedChild, StdioSource};
use serde::Serialize;

const MAX_CAPTURE_BYTES: u64 = 1024 * 1024 * 1024;
const CHUNK_BYTES: usize = 8192;

pub(crate) enum Event {
    Data(Vec<u8>),
    Eof,
    Error,
}

#[derive(Serialize)]
pub(crate) struct CaptureResult {
    schema_version: u8,
    pub(crate) artifact: PathBuf,
    pub(crate) state: &'static str,
    pub(crate) exit_code: Option<i32>,
    pub(crate) bytes: Option<u64>,
    pub(crate) pipe_eof: bool,
    pub(crate) child_exited: bool,
    pub(crate) termination_requested: Option<bool>,
    pub(crate) tree_cleanup: &'static str,
}

/// Capture stdout and stderr to one verified file without passing that file to
/// the child. Success requires both a terminal child status and pipe EOF.
pub(crate) fn capture(
    root: &Path,
    timeout: Duration,
    command: &[String],
) -> io::Result<CaptureResult> {
    capture_with_limit(root, timeout, command, MAX_CAPTURE_BYTES)
}

pub(crate) fn capture_with_limit(
    root: &Path,
    timeout: Duration,
    command: &[String],
    limit: u64,
) -> io::Result<CaptureResult> {
    capture_with_factory(root, timeout, command, limit, create_artifact)
}

// The private factory is fixed by the production entry point.
fn capture_with_factory(
    root: &Path,
    timeout: Duration,
    command: &[String],
    limit: u64,
    create: impl FnOnce(&Path) -> io::Result<(File, PathBuf)>,
) -> io::Result<CaptureResult> {
    if command.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing command",
        ));
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let (mut artifact, path) = create(root)?;
    let (mut child, (rx, stop, reader_handle)) = spawn_capture(command)?;
    let mut exit_code = None;
    let mut eof = false;
    let mut bytes = 0u64;
    let mut state = "complete";
    while exit_code.is_none() || !eof {
        if Instant::now() >= deadline {
            state = "timeout";
            break;
        }
        if exit_code.is_none() {
            match child.try_wait() {
                Ok(status) => exit_code = status,
                Err(_) => {
                    state = "wait_error";
                    break;
                }
            }
        }
        if eof {
            std::thread::sleep(Duration::from_millis(25));
            continue;
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(Event::Data(data)) => {
                let remaining = limit.saturating_sub(bytes);
                let write_len = usize::try_from(remaining)
                    .unwrap_or(usize::MAX)
                    .min(data.len());
                if artifact.write_all(&data[..write_len]).is_err() {
                    state = "write_error";
                    break;
                }
                bytes += write_len as u64;
                if write_len < data.len() {
                    state = "output_limit";
                    break;
                }
            }
            Ok(Event::Eof) => eof = true,
            Ok(Event::Error) => {
                state = "read_error";
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) if eof => {}
            Err(RecvTimeoutError::Disconnected) => {
                state = "read_error";
                break;
            }
        }
    }
    if artifact.flush().is_err() {
        state = "write_error";
    }
    let actual_bytes = artifact.metadata().ok().map(|metadata| metadata.len());
    if actual_bytes.is_none() && state == "complete" {
        state = "artifact_metadata_error";
    }
    let termination_requested = if state == "complete" {
        None
    } else {
        Some(stop_incomplete(&mut child, &rx, &mut exit_code, &mut eof))
    };
    stop.store(true, Ordering::Release);
    drop(rx);
    if reader_handle.join().is_err() && state == "complete" {
        state = "read_error";
    }
    Ok(CaptureResult {
        schema_version: 1,
        artifact: path,
        state,
        exit_code,
        bytes: actual_bytes,
        pipe_eof: eof,
        child_exited: exit_code.is_some(),
        termination_requested,
        tree_cleanup: "unverified",
    })
}

// Native platform tests exercise the lifecycle before enabling a new factory.
#[cfg(test)]
#[allow(
    dead_code,
    reason = "used by native platform tests where private capture is under development"
)]
pub(crate) fn capture_with_test_factory(
    root: &Path,
    timeout: Duration,
    command: &[String],
    limit: u64,
    create: impl FnOnce(&Path) -> io::Result<(File, PathBuf)>,
) -> io::Result<CaptureResult> {
    capture_with_factory(root, timeout, command, limit, create)
}

fn spawn_capture(command: &[String]) -> io::Result<(SpawnedChild, CaptureReader)> {
    let (reader, writer) = os_pipe::pipe()?;
    let writer = crate::platform::test_capture::pipe_writer_file(writer);
    let argv: Vec<OsString> = command.iter().map(OsString::from).collect();
    let stdio = SpawnStdio {
        stdin: StdioSource::Parent,
        stdout: StdioSource::File(&writer),
        stderr: StdioSource::File(&writer),
        ..SpawnStdio::default()
    };
    let (rx, stop, reader_handle) = capture_reader(reader)?;
    let child = match spawn_argv_with_stdio(&argv, None, None, stdio) {
        Ok(child) => child,
        Err(error) => {
            stop.store(true, Ordering::Release);
            drop(writer);
            drop(rx);
            let _ = reader_handle.join();
            return Err(error);
        }
    };
    drop(writer);
    Ok((child, (rx, stop, reader_handle)))
}

fn create_artifact(root: &Path) -> io::Result<(File, PathBuf)> {
    crate::platform::test_capture::verify_root(root)?;
    let temporary = tempfile::Builder::new()
        .prefix("local-test-")
        .suffix(".log")
        .tempfile_in(root)?;
    let (artifact, path) = temporary.keep().map_err(|error| error.error)?;
    crate::platform::test_capture::verify_file(&artifact)?;
    Ok((artifact, path))
}

pub(crate) type CaptureReader = (mpsc::Receiver<Event>, Arc<AtomicBool>, JoinHandle<()>);

pub(crate) fn capture_reader(mut reader: os_pipe::PipeReader) -> io::Result<CaptureReader> {
    let (tx, rx) = mpsc::sync_channel::<Event>(4);
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("clud-test-capture".into())
        .spawn(move || {
            while !reader_stop.load(Ordering::Acquire) {
                let mut data = vec![0; CHUNK_BYTES];
                match crate::platform::test_capture::read_pipe(&mut reader, &mut data) {
                    Ok(None) => continue,
                    Ok(Some(0)) => {
                        let _ = tx.send(Event::Eof);
                        break;
                    }
                    Ok(Some(n)) => {
                        data.truncate(n);
                        if tx.send(Event::Data(data)).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        let _ = tx.send(Event::Error);
                        break;
                    }
                }
            }
        })?;
    Ok((rx, stop, handle))
}

fn stop_incomplete(
    child: &mut SpawnedChild,
    rx: &mpsc::Receiver<Event>,
    exit_code: &mut Option<i32>,
    eof: &mut bool,
) -> bool {
    let requested = child.kill_tree().is_ok();
    let grace = Instant::now() + Duration::from_millis(500);
    while Instant::now() < grace && (exit_code.is_none() || !*eof) {
        if exit_code.is_none() {
            if let Ok(status) = child.try_wait() {
                *exit_code = status;
            }
        }
        if *eof {
            std::thread::sleep(Duration::from_millis(25));
        } else {
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(Event::Eof) => *eof = true,
                Ok(Event::Error) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Event::Data(_)) | Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
    requested
}

/// Internal JSON interface consumed by the bundled report tool.
pub fn run_cli(root: &Path, timeout_secs: u64, command: &[String]) -> i32 {
    let path = root.to_string_lossy();
    if path.len() > 200 || path.chars().any(|c| c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{206f}')) {
        eprintln!("clud test capture: invalid artifact root");
        return 2;
    }
    if timeout_secs == 0 || timeout_secs > 86_400 {
        eprintln!("clud test capture: invalid timeout");
        return 2;
    }
    match capture(root, Duration::from_secs(timeout_secs), command) {
        Ok(result) => match serde_json::to_string(&result) {
            Ok(json) => {
                println!("{json}");
                0
            }
            Err(_) => {
                eprintln!("clud test capture: failed to encode result");
                1
            }
        },
        Err(_) => {
            eprintln!("clud test capture: unable to capture command safely");
            1
        }
    }
}

//! Tee writer for `clud tool run`. Slice 2 of #427.
//!
//! Drains `running_process::NativeProcess::captured_combined()` periodically
//! and writes each `StreamEvent` to:
//!
//! 1. The caller's real stdout or stderr (byte-faithful passthrough so the
//!    user sees the output live, modulo the poll interval).
//! 2. `<log_dir>/stdout.jsonl` or `<log_dir>/stderr.jsonl` — the
//!    per-stream JSONL log.
//! 3. `<log_dir>/combined.jsonl` — the time-ordered merged log.
//!
//! Each JSONL line follows the schema-versioned shape:
//!
//! ```jsonl
//! {"v":1,"ts_ms":1700000000000,"stream":"stdout","bytes":"...base64..."}
//! ```
//!
//! Bytes are base64-encoded because terminal output can contain arbitrary
//! binary (ANSI escapes, UTF-8 partial sequences across chunk boundaries).
//! Encoding once keeps the JSONL parseable without forcing the producer to
//! flush only at line boundaries.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine;
use running_process::{StreamEvent, StreamKind};
use serde_json::json;

/// JSONL schema version for tee log lines. Bumped only by intentional
/// schema changes; readers must tolerate unknown keys within a version.
pub const TEE_SCHEMA_VERSION: u32 = 1;

/// Owns the three JSONL log files plus the caller's stdout/stderr handles
/// for one tool invocation. Built by [`TeeWriter::open`] inside the
/// per-invocation log directory.
pub struct TeeWriter {
    stdout_log: File,
    stderr_log: File,
    combined_log: File,
}

impl TeeWriter {
    /// Open (or create) the three JSONL files under `log_dir`. The dir
    /// is created if missing.
    pub fn open(log_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(log_dir)?;
        Ok(Self {
            stdout_log: open_append(&log_dir.join("stdout.jsonl"))?,
            stderr_log: open_append(&log_dir.join("stderr.jsonl"))?,
            combined_log: open_append(&log_dir.join("combined.jsonl"))?,
        })
    }

    /// Route one `StreamEvent`:
    ///
    /// 1. Write bytes to the caller's matching real stream.
    /// 2. Append a JSONL line to the matching per-stream log.
    /// 3. Append the same line to the combined log.
    pub fn emit(&mut self, event: &StreamEvent) -> io::Result<()> {
        let ts_ms = unix_millis_now();
        let stream_name = match event.stream {
            StreamKind::Stdout => "stdout",
            StreamKind::Stderr => "stderr",
        };
        // Passthrough to the caller's real terminal first so the user sees
        // output without waiting for the JSONL flush.
        match event.stream {
            StreamKind::Stdout => {
                let mut out = io::stdout().lock();
                out.write_all(&event.line)?;
                out.flush()?;
            }
            StreamKind::Stderr => {
                let mut err = io::stderr().lock();
                err.write_all(&event.line)?;
                err.flush()?;
            }
        }
        // Single buffered `write_all` per the #373 race-fix pattern so a
        // concurrent reader doing `read_to_string` cannot hit EOF mid-object.
        let encoded = STANDARD_NO_PAD.encode(&event.line);
        let value = json!({
            "v": TEE_SCHEMA_VERSION,
            "ts_ms": ts_ms,
            "stream": stream_name,
            "bytes": encoded,
        });
        let mut buf = serde_json::to_vec(&value)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        buf.push(b'\n');
        match event.stream {
            StreamKind::Stdout => self.stdout_log.write_all(&buf)?,
            StreamKind::Stderr => self.stderr_log.write_all(&buf)?,
        };
        self.combined_log.write_all(&buf)?;
        Ok(())
    }

    /// Drain a batch of events in order. Returns the number successfully
    /// emitted. Stops on the first emit error and returns it so the caller
    /// can decide whether to keep going.
    pub fn emit_batch(&mut self, events: &[StreamEvent]) -> io::Result<usize> {
        let mut n = 0;
        for event in events {
            self.emit(event)?;
            n += 1;
        }
        Ok(n)
    }

    /// Emit logical lines captured by `running-process`. Its capture API
    /// removes line terminators, so restore them before writing to the caller
    /// and JSONL logs. Without this, downstream line readers buffer until the
    /// tool exits even though clud drains output throughout the run.
    pub fn emit_captured_batch(&mut self, events: &[StreamEvent]) -> io::Result<usize> {
        let mut n = 0;
        for event in events {
            let mut terminated = event.clone();
            terminated.line.push(b'\n');
            self.emit(&terminated)?;
            n += 1;
        }
        Ok(n)
    }

    /// Flush all three JSONL files. Called at the very end of the tool
    /// invocation so the on-disk log is intact even if the parent dies
    /// immediately afterward.
    pub fn flush(&mut self) -> io::Result<()> {
        self.stdout_log.flush()?;
        self.stderr_log.flush()?;
        self.combined_log.flush()
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn unix_millis_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD_NO_PAD;
    use base64::Engine;
    use kernal_api::platform::fs::TemporaryDirectory;

    fn read_lines(path: &Path) -> Vec<serde_json::Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn open_creates_log_dir() {
        let tmp = TemporaryDirectory::new().unwrap();
        let log_dir = tmp.path().join("nested").join("does_not_exist");
        let _w = TeeWriter::open(&log_dir).unwrap();
        assert!(log_dir.join("stdout.jsonl").exists());
        assert!(log_dir.join("stderr.jsonl").exists());
        assert!(log_dir.join("combined.jsonl").exists());
    }

    #[test]
    fn emit_writes_one_jsonl_line_per_call() {
        let tmp = TemporaryDirectory::new().unwrap();
        let log_dir = tmp.path();
        let mut w = TeeWriter::open(log_dir).unwrap();
        w.emit(&StreamEvent {
            stream: StreamKind::Stdout,
            line: b"hello\n".to_vec(),
        })
        .unwrap();
        w.flush().unwrap();
        let stdout_lines = read_lines(&log_dir.join("stdout.jsonl"));
        assert_eq!(stdout_lines.len(), 1);
        assert_eq!(stdout_lines[0]["v"], 1);
        assert_eq!(stdout_lines[0]["stream"], "stdout");
        let encoded = stdout_lines[0]["bytes"].as_str().unwrap();
        let decoded = STANDARD_NO_PAD.decode(encoded).unwrap();
        assert_eq!(decoded, b"hello\n");
        // Stderr log exists but is empty.
        assert!(log_dir.join("stderr.jsonl").exists());
        assert!(read_lines(&log_dir.join("stderr.jsonl")).is_empty());
    }

    #[test]
    fn emit_routes_stderr_separately_and_into_combined() {
        let tmp = TemporaryDirectory::new().unwrap();
        let log_dir = tmp.path();
        let mut w = TeeWriter::open(log_dir).unwrap();
        w.emit(&StreamEvent {
            stream: StreamKind::Stdout,
            line: b"out".to_vec(),
        })
        .unwrap();
        w.emit(&StreamEvent {
            stream: StreamKind::Stderr,
            line: b"err".to_vec(),
        })
        .unwrap();
        w.flush().unwrap();
        let stdout = read_lines(&log_dir.join("stdout.jsonl"));
        let stderr = read_lines(&log_dir.join("stderr.jsonl"));
        let combined = read_lines(&log_dir.join("combined.jsonl"));
        assert_eq!(stdout.len(), 1);
        assert_eq!(stderr.len(), 1);
        assert_eq!(
            combined.len(),
            2,
            "combined holds events from both streams in order"
        );
        assert_eq!(combined[0]["stream"], "stdout");
        assert_eq!(combined[1]["stream"], "stderr");
    }

    #[test]
    fn emit_batch_handles_multiple_events_in_order() {
        let tmp = TemporaryDirectory::new().unwrap();
        let log_dir = tmp.path();
        let mut w = TeeWriter::open(log_dir).unwrap();
        let events = vec![
            StreamEvent {
                stream: StreamKind::Stdout,
                line: b"line1\n".to_vec(),
            },
            StreamEvent {
                stream: StreamKind::Stdout,
                line: b"line2\n".to_vec(),
            },
            StreamEvent {
                stream: StreamKind::Stderr,
                line: b"warning\n".to_vec(),
            },
        ];
        let n = w.emit_batch(&events).unwrap();
        assert_eq!(n, 3);
        w.flush().unwrap();
        let combined = read_lines(&log_dir.join("combined.jsonl"));
        assert_eq!(combined.len(), 3);
        let stdout = read_lines(&log_dir.join("stdout.jsonl"));
        assert_eq!(stdout.len(), 2);
        let stderr = read_lines(&log_dir.join("stderr.jsonl"));
        assert_eq!(stderr.len(), 1);
    }

    #[test]
    fn emit_captured_batch_restores_stdout_and_stderr_delimiters() {
        let tmp = TemporaryDirectory::new().unwrap();
        let log_dir = tmp.path();
        let mut w = TeeWriter::open(log_dir).unwrap();
        let events = vec![
            StreamEvent {
                stream: StreamKind::Stdout,
                line: b"first".to_vec(),
            },
            StreamEvent {
                stream: StreamKind::Stderr,
                line: b"second".to_vec(),
            },
        ];

        assert_eq!(w.emit_captured_batch(&events).unwrap(), 2);
        w.flush().unwrap();

        let combined = read_lines(&log_dir.join("combined.jsonl"));
        let first = STANDARD_NO_PAD
            .decode(combined[0]["bytes"].as_str().unwrap())
            .unwrap();
        let second = STANDARD_NO_PAD
            .decode(combined[1]["bytes"].as_str().unwrap())
            .unwrap();
        assert_eq!(first, b"first\n");
        assert_eq!(second, b"second\n");
        assert_eq!(combined[0]["stream"], "stdout");
        assert_eq!(combined[1]["stream"], "stderr");
    }

    #[test]
    fn emit_handles_arbitrary_binary_bytes() {
        let tmp = TemporaryDirectory::new().unwrap();
        let log_dir = tmp.path();
        let mut w = TeeWriter::open(log_dir).unwrap();
        // ANSI escape + partial UTF-8 sequence + raw bytes.
        let blob = b"\x1b[31mred\x1b[0m\xc3\xa9\x00\xff";
        w.emit(&StreamEvent {
            stream: StreamKind::Stdout,
            line: blob.to_vec(),
        })
        .unwrap();
        w.flush().unwrap();
        let stdout = read_lines(&log_dir.join("stdout.jsonl"));
        assert_eq!(stdout.len(), 1);
        let decoded = STANDARD_NO_PAD
            .decode(stdout[0]["bytes"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, blob);
    }
}

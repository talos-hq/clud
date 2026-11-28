//! End-to-end input fidelity sweep (#1697 follow-up): everything a real
//! terminal sends to clud must reach the child byte-for-byte, however the
//! input source happens to chunk it.
//!
//! #1697 got past a subsystem-by-subsystem audit (#1343) because no test
//! pushed real terminal traffic through the real pump into a real PTY with
//! hostile chunking. This sweep does, on every platform CI runs: a corpus of
//! terminal-originated sequences (mouse, focus, bracketed paste, query
//! replies, function and modified keys, kitty CSI-u keys, Alt+key, UTF-8 and
//! emoji) goes through both production input paths — the console-input
//! channel (`extra_rx`, Windows' interactive path) and the byte-stream stdin
//! reader (POSIX's) — whole, one byte per write, and in an awkward stride.
//! `mock-agent` reads with VT input like Claude Code and records exactly what
//! it got; the only transforms allowed are ConPTY's own (LF→CR, and focus
//! reports consumed): see [`expected_for`].

use std::io::{Cursor, Read};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::Duration;

use running_process::pty::NativePtyProcess;

use crate::common::{drain_reader, mock_agent_path, through_pty_input, wait_for_mock_ready};

/// What terminals send: one entry per class, each a complete sequence.
const CORPUS: &[(&str, &[u8])] = &[
    ("text", b"plain text 123"),
    ("enter", b"\r"),
    ("sgr mouse press/release", b"\x1b[<0;10;5M\x1b[<0;10;5m"),
    ("sgr mouse motion", b"\x1b[<35;31;18M\x1b[<35;33;20M"),
    ("sgr mouse wheel", b"\x1b[<64;5;5M\x1b[<65;5;5M"),
    ("focus in", b"\x1b[I"),
    ("focus out", b"\x1b[O"),
    ("arrows", b"\x1b[A\x1b[B\x1b[C\x1b[D"),
    ("modified arrow", b"\x1b[1;5C"),
    ("ss3 function key", b"\x1bOP"),
    ("tilde function key", b"\x1b[15~"),
    ("cursor position reply", b"\x1b[12;40R"),
    ("device attributes reply", b"\x1b[?1;2c"),
    ("kitty csi-u key", b"\x1b[97;5u"),
    ("alt+x", b"\x1bx"),
    (
        "bracketed paste",
        b"\x1b[200~first line\nsecond line\x1b[201~",
    ),
    ("utf-8", "h\u{e9}llo \u{65e5}\u{672c}\u{8a9e}".as_bytes()),
    ("emoji", "\u{1f600}".as_bytes()),
];

/// Sent after the corpus; mock-agent stops reading once it sees it.
const END: &str = "<<END>>";

/// The corpus as one stream, with a visible separator so a mismatch names
/// the entry it is in, then [`END`].
fn corpus_stream() -> Vec<u8> {
    let mut stream = Vec::new();
    for (_, bytes) in CORPUS {
        stream.extend_from_slice(bytes);
        stream.push(b'|');
    }
    stream.extend_from_slice(END.as_bytes());
    stream
}

/// How an input source hands the stream to the pump.
#[derive(Clone, Copy, Debug)]
enum Chunking {
    /// One chunk per corpus entry: how a terminal writes a keystroke.
    PerEntry,
    /// One byte per chunk: how the Windows console reader delivers a
    /// terminal-originated sequence under VT input (#1697).
    PerByte,
    /// Three bytes per chunk, cutting sequences at awkward offsets.
    Stride3,
}

fn chunks(chunking: Chunking) -> Vec<Vec<u8>> {
    let stream = corpus_stream();
    match chunking {
        Chunking::PerEntry => CORPUS
            .iter()
            .map(|(_, bytes)| {
                let mut chunk = bytes.to_vec();
                chunk.push(b'|');
                chunk
            })
            .chain(std::iter::once(END.as_bytes().to_vec()))
            .collect(),
        Chunking::PerByte => stream.chunks(1).map(<[u8]>::to_vec).collect(),
        Chunking::Stride3 => stream.chunks(3).map(<[u8]>::to_vec).collect(),
    }
}

/// Which production input path carries the stream.
#[derive(Clone, Copy, Debug)]
enum Path {
    /// `extra_rx`: the Windows console-input reader's channel.
    ConsoleChannel,
    /// The byte-stream stdin reader POSIX uses.
    ByteStream,
}

struct NoHooks;

impl clud::session::InteractiveHooks for NoHooks {
    fn intercept_f3(&self) -> bool {
        false
    }
    fn on_f3_press(&mut self, _sink: &mut dyn clud::session::PtyInputSink) -> std::io::Result<()> {
        Ok(())
    }
    fn on_f3_release(
        &mut self,
        _sink: &mut dyn clud::session::PtyInputSink,
    ) -> std::io::Result<()> {
        Ok(())
    }
    fn on_tick(&mut self, _sink: &mut dyn clud::session::PtyInputSink) -> std::io::Result<()> {
        Ok(())
    }
}

/// The pause between two chunks of input, longer than the 5 ms after which a
/// held lone Esc is released (`HUMAN_PREFIX_FLUSH`): input that arrives in
/// pieces this far apart (a slow link, a loaded machine, Windows' ~15 ms
/// timer granularity) must still reach the child whole. Releasing a partial
/// report after that same 5 ms lost it on Windows (#1717).
const INPUT_GAP: Duration = Duration::from_millis(10);

/// A stdin that returns one prepared chunk per `read`, like a terminal.
struct ChunkedStdin {
    chunks: std::collections::VecDeque<Vec<u8>>,
}

impl Read for ChunkedStdin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // EOF once drained: the pump's reader just stops (no EOF reaches the
        // child).
        let Some(chunk) = self.chunks.pop_front() else {
            return Ok(0);
        };
        let n = chunk.len().min(buf.len());
        buf[..n].copy_from_slice(&chunk[..n]);
        if n < chunk.len() {
            self.chunks.push_front(chunk[n..].to_vec());
        }
        std::thread::sleep(INPUT_GAP);
        Ok(n)
    }
}

/// Run one path × chunking through a real PTY into mock-agent; what it read.
fn child_reads(path: Path, chunking: Chunking) -> Vec<u8> {
    let agent = mock_agent_path();
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let raw_stdin = tmp.path().join("stdin_raw.bin");
    let ready = tmp.path().join("ready");
    let argv = vec![
        agent.to_string_lossy().to_string(),
        // A ceiling only: the read ends at END. One byte at a time, a loaded
        // macOS runner overshot a fixed 8 s window.
        "--mock-read-stdin-ms".to_string(),
        "45000".to_string(),
        "--mock-read-stdin-until".to_string(),
        END.to_string(),
        "--mock-stdin-raw-to".to_string(),
        raw_stdin.to_string_lossy().to_string(),
        "--mock-ready-file".to_string(),
        ready.to_string_lossy().to_string(),
    ];
    let process = NativePtyProcess::new(argv, None, None, 24, 80, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start");
    wait_for_mock_ready(&process, &ready);

    let interrupted = AtomicBool::new(false);
    let mut hooks = NoHooks;
    let pieces = chunks(chunking);
    match path {
        Path::ConsoleChannel => {
            let (tx, rx) = mpsc::channel::<Vec<u8>>();
            let feeder = std::thread::spawn(move || {
                for piece in pieces {
                    if tx.send(piece).is_err() {
                        break;
                    }
                    std::thread::sleep(INPUT_GAP);
                }
            });
            let _exit = clud::session::run_raw_pty_pump_with_extra_rx(
                &process,
                &interrupted,
                &mut hooks,
                Cursor::new(Vec::<u8>::new()),
                Some(rx),
            );
            let _ = feeder.join();
        }
        Path::ByteStream => {
            let stdin = ChunkedStdin {
                chunks: pieces.into(),
            };
            let _exit = clud::session::run_raw_pty_pump(&process, &interrupted, &mut hooks, stdin);
        }
    }
    let _ = process.wait_impl(Some(5.0));
    let _ = drain_reader(&process, Duration::from_millis(300));
    let _ = process.close_impl();
    std::fs::read(&raw_stdin).unwrap_or_default()
}

/// What the child may read for one corpus entry; any one of these is right.
/// Besides ConPTY's LF→CR ([`through_pty_input`]), ConPTY consumes a terminal
/// focus report (`ESC [ I` / `ESC [ O`) from its input instead of passing it
/// to the child when the report arrives in one write (observed on the Windows
/// x64 and arm runners, #1717). Cut across writes with idle gaps (per-byte,
/// or a stride that splits it), it depends on timing (Windows timers tick at
/// ~15.6 ms): if the held `ESC` / `ESC [` is released as an Esc / Alt+[
/// keypress before the rest arrives, ConPTY sees the rest as text and the
/// child reads the bytes unchanged; otherwise the report assembles and is
/// consumed. Both are right, and each report resolves on its own. Claude Code run directly under Windows Terminal goes
/// through ConPTY too, so this is the platform's behaviour, not clud's.
fn expected_for(name: &str, bytes: &[u8], chunking: Chunking) -> Vec<Vec<u8>> {
    let intact = through_pty_input(bytes);
    if clud::platform::host::WINDOWS && name.starts_with("focus") {
        return match chunking {
            // One write: ConPTY sees the whole report and consumes it.
            Chunking::PerEntry => vec![Vec::new()],
            // The report may be cut across writes; see above.
            Chunking::PerByte | Chunking::Stride3 => vec![intact, Vec::new()],
        };
    }
    vec![intact]
}

/// The first corpus entry whose bytes the child did not get as expected.
fn first_damaged_entry(got: &[u8], chunking: Chunking) -> Option<String> {
    let mut rest = got;
    'entries: for (name, bytes) in CORPUS {
        let options = expected_for(name, bytes, chunking);
        for option in &options {
            let mut expected = option.clone();
            expected.push(b'|');
            if let Some(after) = rest.strip_prefix(expected.as_slice()) {
                rest = after;
                continue 'entries;
            }
        }
        let longest = options.iter().map(Vec::len).max().unwrap_or(0);
        let shown = &rest[..rest.len().min(longest + 9)];
        let options: Vec<_> = options
            .iter()
            .map(|option| String::from_utf8_lossy(option).into_owned())
            .collect();
        return Some(format!(
            "{name}: expected one of {options:?} then '|', child read {:?}",
            String::from_utf8_lossy(shown)
        ));
    }
    (rest != END.as_bytes()).then(|| {
        format!(
            "after the corpus: expected {END:?}, child read {:?}",
            String::from_utf8_lossy(rest)
        )
    })
}

fn assert_round_trip(path: Path, chunking: Chunking) {
    let got = child_reads(path, chunking);
    if let Some(damage) = first_damaged_entry(&got, chunking) {
        panic!("{path:?} / {chunking:?}: {damage}");
    }
}

#[test]
fn console_channel_per_entry() {
    require_pty_or_skip!("console_channel_per_entry");
    assert_round_trip(Path::ConsoleChannel, Chunking::PerEntry);
}

#[test]
fn console_channel_per_byte() {
    require_pty_or_skip!("console_channel_per_byte");
    assert_round_trip(Path::ConsoleChannel, Chunking::PerByte);
}

#[test]
fn console_channel_stride_3() {
    require_pty_or_skip!("console_channel_stride_3");
    assert_round_trip(Path::ConsoleChannel, Chunking::Stride3);
}

#[test]
fn byte_stream_per_entry() {
    require_pty_or_skip!("byte_stream_per_entry");
    assert_round_trip(Path::ByteStream, Chunking::PerEntry);
}

#[test]
fn byte_stream_per_byte() {
    require_pty_or_skip!("byte_stream_per_byte");
    assert_round_trip(Path::ByteStream, Chunking::PerByte);
}

#[test]
fn byte_stream_stride_3() {
    require_pty_or_skip!("byte_stream_stride_3");
    assert_round_trip(Path::ByteStream, Chunking::Stride3);
}

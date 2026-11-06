//! The client's own view of a remote session: operator-shell output and notices.
//!
//! The person who consented to remote control must be able to watch it, so PTY
//! output is mirrored here as well as sent to the server. One worker owns stdout
//! for the session: a stalled terminal blocks inside a write while holding the
//! stdout lock, and neither mirroring nor a notice may then stall the session
//! loop, which is what handles disconnect and Ctrl+C.
use crate::session::KeyboardEnhancementTracker;
use crate::terminal_queries::TerminalQueryScanner;
use std::io::Write;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::time::Duration;

/// Mirrored output allowed to wait for the local terminal, matching the
/// forwarding queue's 2 MiB. Beyond it the mirror drops output and says so; a
/// slow local terminal must not end the session or delay the operator.
const MAX_PENDING_BYTES: usize = 2 * 1024 * 1024;
const SETTLE_WAIT: Duration = Duration::from_millis(250);
const FINAL_WAIT: Duration = Duration::from_secs(1);

enum Command {
    Output(Vec<u8>),
    Line(String),
    Settle(mpsc::SyncSender<()>),
}

/// Terminal state of the mirrored stream, applied on the worker thread.
struct Renderer<W: Write> {
    sink: W,
    modes: KeyboardEnhancementTracker,
    queries: TerminalQueryScanner,
    at_line_start: bool,
}

impl<W: Write> Renderer<W> {
    fn new(sink: W) -> Self {
        Self {
            sink,
            modes: KeyboardEnhancementTracker::default(),
            queries: TerminalQueryScanner::default(),
            at_line_start: true,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        // A closed stdout must not end a session the operator is still using.
        let _ = self.sink.write_all(bytes);
        let _ = self.sink.flush();
    }

    fn fresh_line(&mut self) {
        if !self.at_line_start {
            self.write(b"\r\n");
            self.at_line_start = true;
        }
    }

    fn line(&mut self, text: &str) {
        self.fresh_line();
        self.write(text.as_bytes());
        self.write(b"\n");
    }

    fn output(&mut self, bytes: &[u8]) {
        self.modes.observe(bytes);
        // The operator's terminal already answers these on the server side. Here
        // the replies would land on stdin, as junk on screen and in a consent answer.
        let visible = self.queries.strip(bytes);
        if let Some(&last) = visible.last() {
            self.at_line_start = last == b'\n';
            self.write(&visible);
        }
    }

    /// Undo what the remote program left on, so what follows is readable.
    fn settle(&mut self) {
        let reset = self.modes.take_child_reset();
        self.write(&reset);
        self.queries = TerminalQueryScanner::default();
        self.fresh_line();
    }
}

fn run_worker<W: Write>(
    sink: W,
    commands: mpsc::Receiver<Command>,
    pending: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
) {
    let mut renderer = Renderer::new(sink);
    for command in commands {
        // Before every command: output may stop for good right after a gap.
        let missing = dropped.swap(0, Ordering::Relaxed);
        if missing > 0 {
            renderer.line(&format!(
                "[clud] {missing} bytes of remote terminal output were not shown here."
            ));
        }
        match command {
            Command::Output(bytes) => {
                renderer.output(&bytes);
                pending.fetch_sub(bytes.len(), Ordering::Relaxed);
            }
            Command::Line(text) => renderer.line(&text),
            Command::Settle(done) => {
                renderer.settle();
                let _ = done.try_send(());
            }
        }
    }
}

pub(super) struct Console {
    commands: mpsc::Sender<Command>,
    pending: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl Console {
    pub fn stdout() -> Self {
        Self::with_sink(std::io::stdout())
    }

    fn with_sink<W: Write + Send + 'static>(sink: W) -> Self {
        let (commands, receiver) = mpsc::channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let worker_pending = Arc::clone(&pending);
        let worker_dropped = Arc::clone(&dropped);
        // Detached: a worker blocked on a stalled terminal must not delay exit.
        std::thread::spawn(move || run_worker(sink, receiver, worker_pending, worker_dropped));
        Self {
            commands,
            pending,
            dropped,
        }
    }

    /// Mirror PTY output. Never blocks; drops output past the pending limit.
    pub fn output(&self, bytes: &[u8]) {
        if self.pending.load(Ordering::Relaxed) + bytes.len() > MAX_PENDING_BYTES {
            self.dropped.fetch_add(bytes.len(), Ordering::Relaxed);
            return;
        }
        self.pending.fetch_add(bytes.len(), Ordering::Relaxed);
        let _ = self.commands.send(Command::Output(bytes.to_vec()));
    }

    /// Print a notice. Notices are few and are never dropped.
    pub fn line(&self, text: impl Into<String>) {
        let _ = self.commands.send(Command::Line(text.into()));
    }

    /// Restore the local terminal once the remote program has ended.
    pub fn settle(&self) {
        self.settle_within(SETTLE_WAIT);
    }

    fn settle_within(&self, wait: Duration) {
        let (done, finished) = mpsc::sync_channel(1);
        if self.commands.send(Command::Settle(done)).is_ok() {
            let _ = finished.recv_timeout(wait);
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.settle_within(FINAL_WAIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    fn rendered(steps: impl FnOnce(&mut Renderer<&mut Vec<u8>>)) -> Vec<u8> {
        let mut out = Vec::new();
        steps(&mut Renderer::new(&mut out));
        out
    }

    #[test]
    fn output_reaches_the_terminal_byte_for_byte() {
        let out = rendered(|r| r.output(b"\x1b[31mhello\x1b[0m\r\n"));
        assert_eq!(out, b"\x1b[31mhello\x1b[0m\r\n");
    }

    #[test]
    fn terminal_queries_are_hidden_even_when_split_across_frames() {
        let out = rendered(|r| {
            r.output(b"a\x1b[6nb\x1b[");
            r.output(b"cd");
        });
        assert_eq!(out, b"abd");
    }

    #[test]
    fn notice_starts_on_its_own_line() {
        let out = rendered(|r| {
            r.output(b"$ ");
            r.line("Operator: hi");
            r.line("next");
        });
        assert_eq!(out, b"$ \r\nOperator: hi\nnext\n");
    }

    #[test]
    fn settle_undoes_only_what_the_remote_left_on_and_only_once() {
        let out = rendered(|r| {
            r.output(b"\x1b[?1049h\x1b[>1u\x1b[?25l");
            r.settle();
            r.settle();
        });
        let expected_prefix = b"\x1b[?1049h\x1b[>1u\x1b[?25l";
        let reset = &out[expected_prefix.len()..];
        assert!(reset.starts_with(b"\x1b[?1049l"), "{reset:?}");
        assert!(reset.windows(5).any(|w| w == b"\x1b[<1u"), "{reset:?}");
        assert!(reset.windows(6).any(|w| w == b"\x1b[?25h"), "{reset:?}");
        assert_eq!(reset.iter().filter(|&&b| b == b'\n').count(), 1);
        assert!(rendered(|r| r.settle()).is_empty());
    }

    /// A terminal that accepts nothing until released, such as one paused by XOFF.
    struct Stalled(mpsc::Receiver<()>, Arc<Mutex<Vec<u8>>>);

    impl Write for Stalled {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            self.1.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stalled_terminal_never_blocks_the_session_and_reports_the_gap() {
        let (release, gate) = mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let console = Console::with_sink(Stalled(gate, Arc::clone(&written)));
        let started = Instant::now();
        let frame = vec![b'x'; 16 * 1024];
        for _ in 0..1000 {
            console.output(&frame);
        }
        console.line("Operator: still here");
        console.settle();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(console.dropped.load(Ordering::Relaxed) > 0);
        drop(release);
        drop(console);
        let text = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
        assert!(text.contains("Operator: still here"), "{}", text.len());
        assert!(text.contains("were not shown here"), "{}", text.len());
    }
}

//! The client's own view of a remote session: operator-shell output and notices.
//!
//! The person who consented to remote control must be able to watch it, so PTY
//! output is mirrored here as well as sent to the server. One worker owns stdout
//! for the session: a stalled terminal blocks inside a write while holding the
//! stdout lock, and neither mirroring nor a notice may then stall the session
//! loop, which is what handles disconnect and Ctrl+C.
use super::mirror::{MirrorFilter, RESET_INPUT_MODES};
use crate::session::KeyboardEnhancementTracker;
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
    Prompt(String),
    Answered { echoed: bool },
    Settle(mpsc::SyncSender<()>),
}

/// Terminal state of the mirrored stream, applied on the worker thread.
struct Renderer<W: Write> {
    sink: W,
    modes: KeyboardEnhancementTracker,
    mirror: MirrorFilter,
    /// Remote output reached the terminal since the last settle.
    mirrored: bool,
    at_line_start: bool,
}

impl<W: Write> Renderer<W> {
    fn new(sink: W) -> Self {
        Self {
            sink,
            modes: KeyboardEnhancementTracker::default(),
            mirror: MirrorFilter::default(),
            mirrored: false,
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

    /// Ask a question (`crate::yes_no::question`) and leave the cursor after
    /// it, where the answer is typed.
    fn prompt(&mut self, text: &str) {
        self.fresh_line();
        self.write(text.as_bytes());
        self.at_line_start = false;
    }

    /// A terminal echoes the answer's Enter, which already ended the prompt line.
    fn answered(&mut self, echoed: bool) {
        if echoed {
            self.at_line_start = true;
        }
    }

    fn output(&mut self, bytes: &[u8]) {
        // The operator's terminal answers queries and reports input on the
        // server side. Here replies and mouse reports would land on stdin, as
        // junk on screen, in a consent answer and in the shell afterwards.
        let visible = self.mirror.filter(bytes);
        self.modes.observe(&visible);
        if let Some(&last) = visible.last() {
            self.mirrored = true;
            self.at_line_start = last == b'\n';
            self.write(&visible);
        }
    }

    /// Undo what the remote program left on, so what follows is readable.
    fn settle(&mut self) {
        let reset = self.modes.take_child_reset();
        self.write(&reset);
        // The filter never forwards an input-reporting mode; this also clears
        // one left on by an older client that did, so the shell stays clean.
        if std::mem::take(&mut self.mirrored) {
            self.write(RESET_INPUT_MODES);
        }
        self.mirror = MirrorFilter::default();
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
            Command::Prompt(text) => renderer.prompt(&text),
            Command::Answered { echoed } => renderer.answered(echoed),
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

    pub(super) fn with_sink<W: Write + Send + 'static>(sink: W) -> Self {
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

    /// Ask a question; the answer is typed on the same line.
    pub fn prompt(&self, text: impl Into<String>) {
        let _ = self.commands.send(Command::Prompt(text.into()));
    }

    /// The prompt was answered. `echoed` is whether a terminal echoed the answer.
    pub fn answered(&self, echoed: bool) {
        let _ = self.commands.send(Command::Answered { echoed });
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
    fn prompt_takes_its_answer_on_the_same_line() {
        let out = rendered(|r| {
            r.output(b"$ ");
            r.prompt("Allow? [y/N]: ");
            r.answered(true);
            r.line("granted");
        });
        assert_eq!(out, b"$ \r\nAllow? [y/N]: granted\n");
    }

    #[test]
    fn notice_after_unanswered_or_unechoed_prompt_starts_a_new_line() {
        let pending = rendered(|r| {
            r.prompt("Allow? [y/N]: ");
            r.line("Operator: hi");
        });
        assert_eq!(pending, b"Allow? [y/N]: \r\nOperator: hi\n");
        let piped = rendered(|r| {
            r.prompt("Allow? [y/N]: ");
            r.answered(false);
            r.line("granted");
        });
        assert_eq!(piped, b"Allow? [y/N]: \r\ngranted\n");
    }

    #[test]
    fn settle_undoes_only_what_the_remote_left_on_and_only_once() {
        let out = rendered(|r| {
            r.output(b"\x1b[?1049h\x1b[>1u\x1b[?1003h\x1b[?25l");
            r.settle();
            r.settle();
        });
        // Keyboard and mouse reporting never reach the local terminal.
        let expected_prefix = b"\x1b[?1049h\x1b[?25l";
        assert!(out.starts_with(expected_prefix), "{out:?}");
        let reset = &out[expected_prefix.len()..];
        assert!(reset.starts_with(b"\x1b[?1049l"), "{reset:?}");
        assert!(!reset.windows(5).any(|w| w == b"\x1b[<1u"), "{reset:?}");
        assert!(reset.windows(6).any(|w| w == b"\x1b[?25h"), "{reset:?}");
        assert!(reset.windows(8).any(|w| w == b"\x1b[?1003l"), "{reset:?}");
        assert_eq!(reset.iter().filter(|&&b| b == b'\n').count(), 1);
        assert!(rendered(|r| r.settle()).is_empty());
    }

    #[test]
    fn mirrored_tui_cannot_make_the_local_terminal_type_into_stdin() {
        // What Claude Code sent when Konsole answered and spewed mouse reports.
        let out = rendered(|r| {
            r.output(b"\x1b[>0q\x1b[?1000;1006h\x1b[?1004h\x1b[?2004h\x1b[c");
            r.output(b"WARNING: Bypass Permissions mode");
        });
        assert_eq!(out, b"WARNING: Bypass Permissions mode");
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
        drop(release);
        drop(console);
        // Drop's wait is bounded, so a loaded machine may still be writing.
        let deadline = Instant::now() + Duration::from_secs(30);
        let shown =
            |needle: &str| String::from_utf8_lossy(&written.lock().unwrap()).contains(needle);
        while !(shown("Operator: still here") && shown("were not shown here"))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(shown("Operator: still here"));
        assert!(shown("were not shown here"));
    }
}

//! The client's own view of a remote session: operator-shell output and notices.
//!
//! The person who consented to remote control must be able to watch it, so PTY
//! output is mirrored here as well as sent to the server. One worker owns stdout
//! for the session: a stalled terminal blocks inside a write while holding the
//! stdout lock, and neither mirroring nor a notice may then stall the session
//! loop, which is what handles disconnect and Ctrl+C.
use super::mirror::{MirrorFilter, RESET_INPUT_MODES};
use crate::session::KeyboardEnhancementTracker;
use std::io::{IsTerminal, Write};
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

/// Draw on the bottom row and come back: save the cursor (and its colours),
/// go to the last row (999 is clamped to it) and erase the row. The text is
/// cut to fit the row, so it never wraps (which would scroll the screen), and
/// the remote program's own wrapping mode is left alone.
const BANNER_OPEN: &[u8] = b"\x1b7\x1b[999;1H\x1b[2K\x1b[7m";
/// Back to normal text and the cursor where the program left it.
const BANNER_CLOSE: &[u8] = b"\x1b[27m\x1b8";
/// Erase the bottom row and return.
const BANNER_ERASE: &[u8] = b"\x1b7\x1b[999;1H\x1b[2K\x1b8";

enum Command {
    Output(Vec<u8>),
    Line(String),
    Prompt(String),
    Answered {
        echoed: bool,
    },
    Hold(bool),
    Settle(mpsc::SyncSender<()>),
    Banner(String),
    /// Clear the banner, or print this line where there is none.
    ClearBanner(String),
}

/// Terminal state of the mirrored stream, applied on the worker thread.
struct Renderer<W: Write> {
    sink: W,
    modes: KeyboardEnhancementTracker,
    mirror: MirrorFilter,
    /// Remote output reached the terminal since the last settle.
    mirrored: bool,
    at_line_start: bool,
    /// The sink is a terminal, so a banner can be drawn over the remote
    /// program's screen and erased again; otherwise banners are lines.
    overlay: bool,
    /// The banner on the bottom row.
    banner: Option<String>,
}

impl<W: Write> Renderer<W> {
    fn new(sink: W) -> Self {
        Self {
            sink,
            modes: KeyboardEnhancementTracker::default(),
            mirror: MirrorFilter::default(),
            mirrored: false,
            at_line_start: true,
            overlay: false,
            banner: None,
        }
    }

    /// A connection notice on the terminal's bottom row, drawn over the
    /// remote program without moving its cursor or scrolling its screen, so
    /// clearing it leaves nothing behind. Not a terminal: an ordinary line.
    fn banner(&mut self, text: &str) {
        if !self.overlay {
            self.line(text);
            return;
        }
        self.banner = Some(text.to_owned());
        self.draw_banner();
    }

    /// Draw the banner, cut to the terminal's width.
    fn draw_banner(&mut self) {
        let Some(text) = &self.banner else {
            return;
        };
        let width = crossterm::terminal::size().map_or(usize::MAX, |(cols, _)| usize::from(cols));
        let fitted: String = text.chars().take(width.saturating_sub(1)).collect();
        let mut bytes = BANNER_OPEN.to_vec();
        bytes.extend_from_slice(fitted.as_bytes());
        bytes.extend_from_slice(BANNER_CLOSE);
        self.write(&bytes);
    }

    /// Take the banner off the screen for a moment: anything written while
    /// it is shown could scroll it up into the remote program's screen,
    /// where the erase on resume would no longer find it.
    fn lift_banner(&mut self) -> bool {
        if self.banner.is_some() {
            self.write(BANNER_ERASE);
        }
        self.banner.is_some()
    }

    /// Erase the banner. Not a terminal: print `done` instead.
    fn clear_banner(&mut self, done: &str) {
        if !self.overlay {
            self.line(done);
        } else if self.banner.take().is_some() {
            self.write(BANNER_ERASE);
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
        let lifted = self.lift_banner();
        self.fresh_line();
        self.write(text.as_bytes());
        self.write(b"\n");
        if lifted {
            self.draw_banner();
        }
    }

    /// Ask a question (`crate::yes_no::question`) and leave the cursor after
    /// it, where the answer is typed.
    fn prompt(&mut self, text: &str) {
        let lifted = self.lift_banner();
        self.fresh_line();
        self.write(text.as_bytes());
        self.at_line_start = false;
        if lifted {
            self.draw_banner();
        }
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
            let lifted = self.lift_banner();
            self.write(&visible);
            if lifted {
                self.draw_banner();
            }
        }
    }

    /// Undo what the remote program left on, so what follows is readable.
    fn settle(&mut self) {
        if self.banner.take().is_some() {
            self.write(BANNER_ERASE);
        }
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
    overlay: bool,
    commands: mpsc::Receiver<Command>,
    pending: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
) {
    let mut renderer = Renderer::new(sink);
    renderer.overlay = overlay;
    // Output held while a local question is open, still counted as pending.
    let mut held: Option<Vec<Vec<u8>>> = None;
    for command in commands {
        // Before every command: output may stop for good right after a gap.
        let missing = dropped.swap(0, Ordering::Relaxed);
        if missing > 0 {
            renderer.line(&format!(
                "[clud] {missing} bytes of remote terminal output were not shown here."
            ));
        }
        match command {
            Command::Output(bytes) => match &mut held {
                Some(chunks) => chunks.push(bytes),
                None => show(&mut renderer, &bytes, &pending),
            },
            Command::Hold(true) => {
                held.get_or_insert_with(Vec::new);
            }
            Command::Hold(false) => {
                for bytes in held.take().unwrap_or_default() {
                    show(&mut renderer, &bytes, &pending);
                }
            }
            Command::Line(text) => renderer.line(&text),
            Command::Prompt(text) => renderer.prompt(&text),
            Command::Answered { echoed } => renderer.answered(echoed),
            Command::Settle(done) => {
                renderer.settle();
                let _ = done.try_send(());
            }
            Command::Banner(text) => renderer.banner(&text),
            Command::ClearBanner(done) => renderer.clear_banner(&done),
        }
    }
}

fn show<W: Write>(renderer: &mut Renderer<W>, bytes: &[u8], pending: &AtomicUsize) {
    renderer.output(bytes);
    pending.fetch_sub(bytes.len(), Ordering::Relaxed);
}

pub(super) struct Console {
    commands: mpsc::Sender<Command>,
    pending: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    /// Banners are drawn over the screen ([`Renderer::banner`]).
    overlay: bool,
}

impl Console {
    pub fn stdout() -> Self {
        Self::with_overlay(std::io::stdout(), std::io::stdout().is_terminal())
    }

    #[cfg(test)]
    pub(super) fn with_sink<W: Write + Send + 'static>(sink: W) -> Self {
        Self::with_overlay(sink, false)
    }

    /// `overlay`: `sink` is a terminal, where banners are drawn and erased.
    pub(super) fn with_overlay<W: Write + Send + 'static>(sink: W, overlay: bool) -> Self {
        let (commands, receiver) = mpsc::channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let worker_pending = Arc::clone(&pending);
        let worker_dropped = Arc::clone(&dropped);
        // Detached: a worker blocked on a stalled terminal must not delay exit.
        std::thread::spawn(move || {
            run_worker(sink, overlay, receiver, worker_pending, worker_dropped);
        });
        Self {
            commands,
            pending,
            dropped,
            overlay,
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

    /// Show a transient connection notice ([`Renderer::banner`]).
    pub fn banner(&self, text: impl Into<String>) {
        let _ = self.commands.send(Command::Banner(text.into()));
    }

    /// Clear the banner; without a terminal print `done` instead. `true`
    /// when a banner may have covered the remote program's screen.
    pub fn clear_banner(&self, done: impl Into<String>) -> bool {
        let _ = self.commands.send(Command::ClearBanner(done.into()));
        self.overlay
    }

    /// Ask a question; the answer is typed on the same line.
    pub fn prompt(&self, text: impl Into<String>) {
        let _ = self.commands.send(Command::Prompt(text.into()));
    }

    /// The prompt was answered. `echoed` is whether a terminal echoed the answer.
    pub fn answered(&self, echoed: bool) {
        let _ = self.commands.send(Command::Answered { echoed });
    }

    /// Hold mirrored output while a local question (the sudo consent and
    /// hidden password) is open, so the operator's output cannot interleave
    /// with it; release shows what was held. Still bounded by the 2 MiB
    /// pending limit, past which output is dropped and the gap reported.
    pub fn hold(&self, on: bool) {
        let _ = self.commands.send(Command::Hold(on));
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

    fn on_terminal(steps: impl FnOnce(&mut Renderer<&mut Vec<u8>>)) -> Vec<u8> {
        rendered(|r| {
            r.overlay = true;
            steps(r);
        })
    }

    #[test]
    fn a_banner_on_a_terminal_is_drawn_over_the_bottom_row_and_erased_without_a_trace() {
        let out = on_terminal(|r| {
            r.output(b"\x1b[?1049h$ top");
            r.banner("lost");
        });
        let drawn = &out[b"\x1b[?1049h$ top".len()..];
        // Saved and restored around the drawing: the program's cursor and
        // colours are where it left them, and nothing scrolled.
        assert!(drawn.starts_with(b"\x1b7\x1b[999;1H\x1b[2K"), "{drawn:?}");
        assert!(drawn.ends_with(b"\x1b8"), "{drawn:?}");
        assert!(!drawn.contains(&b'\n'), "{drawn:?}");
        assert!(drawn.windows(4).any(|w| w == b"lost"));
        let cleared = on_terminal(|r| {
            r.banner("lost");
            r.clear_banner("Reconnected.");
            r.clear_banner("Reconnected.");
        });
        let erase = cleared
            .strip_prefix(&out[b"\x1b[?1049h$ top".len()..])
            .unwrap();
        assert_eq!(erase, BANNER_ERASE, "erased once, no text left behind");
    }

    #[test]
    fn writing_while_the_banner_shows_lifts_it_first_so_it_never_scrolls_into_the_screen() {
        let out = on_terminal(|r| {
            r.banner("lost");
            r.output(b"one\r\ntwo\r\n");
            r.line("Operator: hi");
        });
        let text = String::from_utf8_lossy(&out).into_owned();
        let erase = String::from_utf8_lossy(BANNER_ERASE).into_owned();
        // Every write is preceded by an erase and followed by a redraw.
        let output_at = text.find("one").unwrap();
        let line_at = text.find("Operator: hi").unwrap();
        let erases: Vec<usize> = text.match_indices(&erase).map(|(i, _)| i).collect();
        assert!(erases.iter().any(|&i| i < output_at), "{text:?}");
        assert!(
            erases.iter().any(|&i| i > output_at && i < line_at),
            "{text:?}"
        );
        assert!(text.ends_with("\x1b[27m\x1b8"), "redrawn last: {text:?}");
        // The program's wrapping mode is never touched.
        assert!(!text.contains("\x1b[?7"), "{text:?}");
    }

    #[test]
    fn a_lost_connection_leaves_the_remote_program_s_modes_alone() {
        // Nothing is reset when the banner appears: the program is still
        // running and drawing, and a reset would break its screen on resume.
        let out = on_terminal(|r| {
            r.output(b"\x1b[?1049h\x1b[?25l");
            r.banner("lost");
        });
        assert!(!out.windows(8).any(|w| w == b"\x1b[?1049l"), "{out:?}");
        assert!(!out.windows(6).any(|w| w == b"\x1b[?25h"), "{out:?}");
    }

    #[test]
    fn settling_erases_a_banner_still_shown() {
        let out = on_terminal(|r| {
            r.banner("lost");
            r.settle();
        });
        assert!(out.windows(BANNER_ERASE.len()).any(|w| w == BANNER_ERASE));
    }

    #[test]
    fn without_a_terminal_banners_are_plain_notice_lines() {
        let out = rendered(|r| {
            r.output(b"$ ");
            r.banner("Connection lost");
            r.clear_banner("Reconnected.");
        });
        assert_eq!(out, b"$ \r\nConnection lost\nReconnected.\n");
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

    #[test]
    fn held_output_waits_for_the_answer_and_then_appears_in_order() {
        let written = Arc::new(Mutex::new(Vec::new()));
        struct Shared(Arc<Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let console = Console::with_sink(Shared(Arc::clone(&written)));
        console.hold(true);
        console.prompt("Allow? [y/N]: ");
        console.output(b"operator-1 ");
        console.output(b"operator-2");
        console.settle();
        let shown = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
        assert!(!shown.contains("operator"), "{shown}");
        console.hold(false);
        console.settle();
        let shown = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
        assert!(shown.contains("operator-1 operator-2"), "{shown}");
        assert_eq!(console.pending.load(Ordering::Relaxed), 0);
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

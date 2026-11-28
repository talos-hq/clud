//! #1942: the screen a terminal shows through clud must be the screen the
//! child drew.
//!
//! A fullscreen TUI (Claude Code's alternate-screen UI) repaints only the
//! cells it believes changed. If any layer between the child and the
//! terminal (the platform PTY, which is ConPTY on Windows, or clud's output
//! relay) drops, reorders or invents a cell, the divergence is never
//! repaired, and the user sees stale fragments that change when the
//! terminal is forced to redraw.
//!
//! The child paints a fullscreen frame. The test resizes through the pump's
//! real resize channel. The child then paints an incremental second frame
//! that relies on cells it drew earlier. The test replays every byte clud
//! wrote into a terminal model and compares it, cell by cell (text and
//! background), with the same frames replayed straight into a model. That
//! is the screen the child intended.

use std::io::{Cursor, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use clud::toast::compositor::{Fallback, ToastPumpOptions};
use clud::toast::tier::ToastTier;
use clud::toast::ToastHub;
use running_process::pty::NativePtyProcess;

use crate::common::mock_agent_path;

const ROWS: u16 = 24;
const COLS: u16 = 100;
const RESIZED_ROWS: u16 = 30;
const RESIZED_COLS: u16 = 90;
const FRAME1_END: &str = "FRAME1-END";
const FRAME2_END: &str = "FRAME2-END";

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

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn snapshot(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }

    fn wait_for(&self, marker: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if String::from_utf8_lossy(&self.snapshot()).contains(marker) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A Claude-Code-shaped fullscreen frame: an indented list on the left, a
/// coloured diff panel on the right, a scroll inside a region, glyphs from
/// the real UI, and an input and status line.
fn frame1() -> Vec<u8> {
    let mut f = String::from("\x1b[?1049h\x1b[H\x1b[2J");
    for row in 1..=18u16 {
        f.push_str(&format!(
            "\x1b[{row};1H  {row}. .idea/item{row}.xml \u{2014} point \u{2192} SDK {}",
            "w".repeat(usize::from(row) * 2)
        ));
        f.push_str(&format!(
            "\x1b[{row};71H\x1b[44m {:<28} \x1b[0m",
            format!("{row} +# diff line {row}")
        ));
    }
    // Scroll the message area the way a viewport scroll does.
    f.push_str("\x1b[1;18r\x1b[18;1H\n\n\n\x1b[r");
    f.push_str("\x1b[16;1H  Backups are at /tmp/pycharm_backup/");
    f.push_str("\x1b[17;1HWhat I need you to verify");
    f.push_str("\x1b[18;1Htype : PythonConfigurationType");
    f.push_str("\x1b[20;1H\x1b[2K\u{276f} yes, start the 07c optimization");
    f.push_str("\x1b[22;1H\x1b[2K\u{23f5}\u{23f5} bypass permissions on");
    f.push_str(" \u{b7} \u{273b} Worked for 1m 26s");
    f.push_str(&format!("\x1b[24;1H{FRAME1_END}"));
    f.into_bytes()
}

/// An incremental repaint after the resize. It closes the panel by erasing
/// to end of line, and rewrites list rows starting at column 3. It relies on
/// columns 1-2 being blank from frame 1, exactly as a cell-diff renderer
/// does.
fn frame2() -> Vec<u8> {
    let mut f = String::new();
    for row in 1..=18u16 {
        f.push_str(&format!("\x1b[{row};71H\x1b[K"));
    }
    f.push_str("\x1b[4;3H4. .gitignore \u{2014} changed .idea/ to .idea/*");
    f.push_str("\x1b[16;1H\x1b[K\x1b[16;3H1. Settings \u{2192} Project");
    f.push_str("\x1b[17;1H\x1b[K\x1b[17;3H2. Run \u{2192} Edit Configurations");
    f.push_str("\x1b[25;1H\x1b[44m\x1b[K\x1b[0m");
    f.push_str(&format!("\x1b[{RESIZED_ROWS};1H{FRAME2_END}"));
    f.into_bytes()
}

/// The child's intended screen: the frames with the resize between them.
fn intended() -> vt100::Parser {
    let mut screen = vt100::Parser::new(ROWS, COLS, 0);
    screen.process(&frame1());
    screen.screen_mut().set_size(RESIZED_ROWS, RESIZED_COLS);
    screen.process(&frame2());
    screen
}

fn cell_mismatches(want: &vt100::Screen, got: &vt100::Screen) -> Vec<String> {
    let mut out = Vec::new();
    for row in 0..RESIZED_ROWS {
        for col in 0..RESIZED_COLS {
            let (w, g) = (want.cell(row, col).unwrap(), got.cell(row, col).unwrap());
            // ConPTY re-renders a never-written cell as a written space; both
            // display as the same blank.
            let text = |c: &vt100::Cell| {
                let s = c.contents();
                if s == " " {
                    String::new()
                } else {
                    s.to_string()
                }
            };
            if (text(w), w.bgcolor()) != (text(g), g.bgcolor()) {
                out.push(format!(
                    "r{row} c{col}: want {:?}/{:?} got {:?}/{:?}",
                    w.contents(),
                    w.bgcolor(),
                    g.contents(),
                    g.bgcolor()
                ));
            }
        }
    }
    out
}

#[test]
fn a_fullscreen_repaint_across_a_resize_reaches_the_terminal_cell_exact() {
    require_pty_or_skip!("a_fullscreen_repaint_across_a_resize_reaches_the_terminal_cell_exact");

    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
    let first = tmp.path().join("frame1.ansi");
    let second = tmp.path().join("frame2.ansi");
    let release = tmp.path().join("release");
    std::fs::write(&first, frame1()).expect("write frame 1");
    std::fs::write(&second, frame2()).expect("write frame 2");
    let argv = vec![
        mock_agent_path().to_string_lossy().to_string(),
        "--mock-ansi-script".into(),
        first.to_string_lossy().to_string(),
        "--mock-wait-for-file".into(),
        release.to_string_lossy().to_string(),
        "--mock-ansi-after-wait".into(),
        second.to_string_lossy().to_string(),
        "--mock-sleep-ms".into(),
        "30000".into(),
    ];
    let process = NativePtyProcess::new(argv, None, None, ROWS, COLS, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start pty child");

    let capture = Capture::default();
    let (resize_tx, resize_rx) = mpsc::channel::<(u16, u16)>();
    let interrupted = Arc::new(AtomicBool::new(false));
    let driver = {
        let capture = capture.clone();
        let interrupted = Arc::clone(&interrupted);
        std::thread::spawn(move || {
            let painted = capture.wait_for(FRAME1_END, Duration::from_secs(20));
            // Bytes before this point were laid out at the old size.
            let split = capture.snapshot().len();
            resize_tx.send((RESIZED_ROWS, RESIZED_COLS)).unwrap();
            std::thread::sleep(Duration::from_millis(500));
            std::fs::write(&release, b"go").unwrap();
            let repainted = capture.wait_for(FRAME2_END, Duration::from_secs(20));
            std::thread::sleep(Duration::from_millis(500));
            let bytes = capture.snapshot();
            interrupted.store(true, Ordering::SeqCst);
            (painted, repainted, split, bytes)
        })
    };
    let mut hooks = NoHooks;
    clud::session::run_raw_pty_pump_with_toasts_for_test(
        &process,
        &interrupted,
        &mut hooks,
        Cursor::new(Vec::<u8>::new()),
        resize_rx,
        capture.clone(),
        ToastPumpOptions {
            process_gear: false,
            hub: ToastHub::new(),
            tier: ToastTier::Off,
            fallback: Fallback::None,
            usage: None,
            rows: ROWS,
            cols: COLS,
            image_id: 0x434c_1942,
        },
    );
    let _ = process.close_impl();
    let (painted, repainted, split, bytes) = driver.join().expect("driver thread");
    assert!(painted, "frame 1 never reached the terminal");
    assert!(repainted, "frame 2 never reached the terminal");

    let mut terminal = vt100::Parser::new(ROWS, COLS, 0);
    terminal.process(&bytes[..split]);
    terminal.screen_mut().set_size(RESIZED_ROWS, RESIZED_COLS);
    terminal.process(&bytes[split..]);

    let want = intended();
    let mismatches = cell_mismatches(want.screen(), terminal.screen());
    assert!(
        mismatches.is_empty(),
        "{} cells differ from the child's intended screen (first 40):\n{}\n\
         --- intended ---\n{}\n--- terminal ---\n{}\n--- raw bytes after resize ---\n{:?}",
        mismatches.len(),
        mismatches
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"),
        want.screen().contents(),
        terminal.screen().contents(),
        String::from_utf8_lossy(&bytes[split..]),
    );
}

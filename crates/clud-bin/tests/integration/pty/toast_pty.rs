//! #1189: toasts composited into a real PTY session.
//!
//! Every test here runs a real child in a real PTY (ConPTY on Windows), pumps
//! it through `clud::session`'s writer thread with a toast compositor, and
//! captures exactly the bytes a terminal would receive. They run on the
//! Linux, macOS and Windows CI lanes; the text-tier test needs a child that
//! can emit escape sequences, so it is Unix-only.
//!
//! The kitty-tier test is the "Linux with kitty" contract: it asserts the
//! kitty graphics commands a kitty terminal receives, and that replaying the
//! stream into a terminal emulator leaves the child's text intact.

use std::io::{Cursor, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use clud::toast::compositor::{Fallback, ToastPumpOptions};
use clud::toast::tier::ToastTier;
use clud::toast::{Severity, Toast, ToastEvent, ToastHub};
use running_process::pty::NativePtyProcess;

const ROWS: u16 = 24;
pub(crate) const COLS: u16 = 100;

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

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A child that prints a marker, stays alive long enough for the toast to be
/// drawn and to expire, then prints a second marker and exits.
fn marker_child() -> Vec<String> {
    if clud::platform::host::WINDOWS {
        vec![
            "cmd.exe".into(),
            "/c".into(),
            "echo child-ready & ping -n 4 127.0.0.1 >nul & echo child-done".into(),
        ]
    } else {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'child-ready\\r\\n'; sleep 3; printf 'child-done\\r\\n'".into(),
        ]
    }
}

/// The child writes nothing during the HUD's appearance, fade, and expiry.
fn quiet_child() -> Vec<String> {
    if clud::platform::host::WINDOWS {
        vec![
            "cmd.exe".into(),
            "/c".into(),
            "echo child-ready & ping -n 6 127.0.0.1 >nul & echo child-done".into(),
        ]
    } else {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'child-ready\\r\\n'; sleep 5; printf 'child-done\\r\\n'".into(),
        ]
    }
}

/// Run `argv` through the composited pump and return everything written.
pub(crate) fn run_composited(
    argv: Vec<String>,
    hub: Arc<ToastHub>,
    tier: ToastTier,
    fallback: Fallback,
) -> Vec<u8> {
    let process = NativePtyProcess::new(argv, None, None, ROWS, COLS, None).expect("new pty");
    process.set_echo(false);
    process.start_impl().expect("start pty child");
    let capture = Capture::default();
    let (_resize_tx, resize_rx) = mpsc::channel::<(u16, u16)>();
    let interrupted = AtomicBool::new(false);
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
            hub,
            tier,
            fallback,
            usage: None,
            rows: ROWS,
            cols: COLS,
            image_id: 0x434c_1189,
        },
    );
    let _ = process.close_impl();
    let bytes = capture.0.lock().unwrap().clone();
    bytes
}

fn graphics_commands(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split("\x1b_G")
        .skip(1)
        .map(|rest| rest.split("\x1b\\").next().unwrap_or("").to_string())
        .collect()
}

pub(crate) fn replay(bytes: &[u8]) -> vt100::Parser {
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(bytes);
    parser
}

pub(crate) fn expiring(text: &str, ttl: Duration) -> ToastEvent {
    ToastEvent::Show(
        Toast::new("cpu", text, Severity::Warn, std::time::Instant::now()).expiring_after(ttl),
    )
}

/// Kitty tier (kitty, Ghostty, WezTerm): the image is uploaded and placed
/// over the text, removed on expiry, freed at exit — and the child's text is
/// intact, because the image never touches cells. Runs on all three OSes.
#[test]
fn kitty_tier_places_and_removes_the_toast_around_live_child_output() {
    require_pty_or_skip!("kitty_tier_places_and_removes_the_toast_around_live_child_output");
    let hub = ToastHub::new();
    hub.publish(expiring("cpu 287 % toast-e2e", Duration::from_millis(1200)));
    let bytes = run_composited(
        marker_child(),
        Arc::clone(&hub),
        ToastTier::Kitty,
        Fallback::None,
    );

    let commands = graphics_commands(&bytes);
    let has = |prefix: &str| commands.iter().any(|cmd| cmd.starts_with(prefix));
    assert!(has("a=t,f=100"), "image never uploaded: {commands:?}");
    assert!(has("a=p,"), "image never placed: {commands:?}");
    assert!(
        has("a=d,d=i,"),
        "placement not removed on expiry: {commands:?}"
    );
    assert!(
        has("a=d,d=I,"),
        "image data not freed at exit: {commands:?}"
    );
    for cmd in &commands {
        let control = cmd.split(';').next().unwrap();
        assert!(
            control.split(',').any(|kv| kv == "q=2"),
            "a terminal reply would reach the child: {control}"
        );
    }

    let screen = replay(&bytes);
    let contents = screen.screen().contents();
    assert!(
        contents.contains("child-ready"),
        "child text corrupted: {contents:?}"
    );
    assert!(
        contents.contains("child-done"),
        "child text corrupted: {contents:?}"
    );
}

#[test]
fn cpu_hud_appears_fades_and_expires_while_the_child_is_quiet() {
    require_pty_or_skip!("cpu_hud_appears_fades_and_expires_while_the_child_is_quiet");
    let hub = ToastHub::new();
    let publisher_hub = Arc::clone(&hub);
    let publisher = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let now = std::time::Instant::now();
        publisher_hub.publish(ToastEvent::Show(
            Toast::new("cpu", "cpu 287 % quiet-e2e", Severity::Warn, now)
                .expiring_after(Duration::from_millis(2500)),
        ));
    });
    let bytes = run_composited(quiet_child(), hub, ToastTier::Kitty, Fallback::None);
    publisher.join().unwrap();
    let stream = String::from_utf8_lossy(&bytes);
    let ready = stream.find("child-ready").expect("child ready marker");
    let first_upload = stream.find("a=t,f=100").expect("CPU HUD appeared");
    let second_upload = stream[first_upload + 1..]
        .find("a=t,f=100")
        .map(|offset| first_upload + 1 + offset)
        .expect("CPU HUD faded after two seconds");
    let removed = stream[second_upload..]
        .find("a=d,d=i")
        .map(|offset| second_upload + offset)
        .expect("CPU HUD expired");
    let done = stream.find("child-done").expect("child done marker");
    assert!(ready < first_upload && first_upload < second_upload && second_upload < removed);
    assert!(
        removed < done,
        "HUD lifecycle must finish before child output resumes"
    );
    let screen = replay(&bytes);
    assert!(screen.screen().contents().contains("child-ready"));
    assert!(screen.screen().contents().contains("child-done"));
}

/// Title fallback (Codex and other harnesses without an in-grid tier): the
/// title stack is pushed, the toast becomes the title, and expiry pops it.
#[test]
fn title_fallback_pushes_and_pops_the_terminal_title() {
    require_pty_or_skip!("title_fallback_pushes_and_pops_the_terminal_title");
    let hub = ToastHub::new();
    hub.publish(expiring("toast-title-e2e", Duration::from_millis(1200)));
    let bytes = run_composited(marker_child(), hub, ToastTier::Fallback, Fallback::Title);
    let text = String::from_utf8_lossy(&bytes);
    let push = text.find("\x1b[22;0t").expect("title stack push");
    let set = text.find("toast-title-e2e").expect("toast title");
    let pop = text.find("\x1b[23;0t").expect("title stack pop on expiry");
    assert!(push < set && set < pop, "push, set, pop order: {text:?}");
    assert!(replay(&bytes).screen().contents().contains("child-done"));
}

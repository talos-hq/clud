use super::super::terminal::Terminal;
use super::*;
use serde_json::json;
use std::time::{Duration, Instant};
use tungstenite::Message;

const LOCAL: (u16, u16) = (40, 120);

fn following(local: Option<(u16, u16)>) -> (SessionSize, mpsc::Sender<(u16, u16)>) {
    let (tx, rx) = mpsc::channel();
    let mut size = SessionSize::with_local(local, rx);
    size.hello(&json!({"type":"hello", "pty_sizing":"client"}));
    (size, tx)
}

fn operator_open() -> Value {
    json!({"type":"pty_open", "pty_id":"size-test", "cols":100, "rows":30})
}

#[test]
fn the_local_terminal_sets_the_size_when_the_server_follows() {
    let (size, _tx) = following(Some(LOCAL));
    assert_eq!(size.governing(), Some(LOCAL));
    let event = size.applied(&operator_open());
    assert_eq!(
        (event["rows"].as_u64(), event["cols"].as_u64()),
        (Some(40), Some(120))
    );
    assert_eq!(event["pty_id"], "size-test");
}

#[test]
fn the_operator_size_stays_without_a_local_terminal() {
    let (size, _tx) = following(None);
    assert_eq!(size.governing(), None);
    assert_eq!(size.applied(&operator_open()), operator_open());
}

/// A server that does not render `pty_size` would fit the console to the
/// operator's window, so the operator's size must still win there.
#[test]
fn the_operator_size_stays_with_a_server_that_does_not_follow() {
    let (mut size, _tx) = following(Some(LOCAL));
    size.hello(&json!({"type":"hello"}));
    assert_eq!(size.governing(), None);
    assert_eq!(size.applied(&operator_open()), operator_open());
    size.hello(&json!({"type":"hello", "pty_sizing":"operator"}));
    assert_eq!(size.governing(), None);
}

#[test]
fn a_local_resize_is_reported_once_with_the_newest_size() {
    let (mut size, tx) = following(Some(LOCAL));
    assert_eq!(size.changed(), None);
    tx.send((50, 150)).unwrap();
    tx.send((45, 140)).unwrap();
    assert_eq!(size.changed(), Some((45, 140)));
    assert_eq!(size.changed(), None);
    // The same size again (a SIGWINCH with no change) is not a resize.
    tx.send((45, 140)).unwrap();
    assert_eq!(size.changed(), None);
    assert_eq!(size.governing(), Some((45, 140)));
}

#[test]
fn a_local_resize_is_recorded_but_not_applied_while_the_operator_sets_the_size() {
    let (tx, rx) = mpsc::channel();
    let mut size = SessionSize::with_local(Some(LOCAL), rx);
    tx.send((50, 150)).unwrap();
    assert_eq!(size.changed(), None);
    size.hello(&json!({"pty_sizing":"client"}));
    assert_eq!(size.governing(), Some((50, 150)));
}

fn read_until(terminal: &mut Terminal, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = String::new();
    while Instant::now() < deadline && !output.contains(needle) {
        match terminal.output().unwrap() {
            Some(Message::Binary(bytes)) => output.push_str(&String::from_utf8_lossy(&bytes[1..])),
            _ => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    output
}

/// The reported corruption: the user's 228x65 terminal mirrored a PTY sized
/// to the operator's browser, so Claude Code's redraws landed on the wrong
/// rows. The shell must see the local terminal's size however the operator
/// opens or resizes it, and follow the local window when it changes.
#[test]
fn the_shell_sees_the_local_terminal_size_not_the_operators() {
    if crate::platform::host::WINDOWS {
        return;
    }
    let (mut size, tx) = following(Some(LOCAL));
    // The fallback environment has only the running clud on PATH.
    let mut terminal = Terminal::default();
    terminal.open(&size.applied(&operator_open())).unwrap();
    terminal
        .input(b"/bin/stty size; echo SIZE_\"ONE\"\n")
        .unwrap();
    let first = read_until(&mut terminal, "SIZE_ONE");
    tx.send((33, 111)).unwrap();
    let resized = size.changed().expect("local resize applies");
    terminal.resize_to(resized).unwrap();
    let announced = terminal.size_event().unwrap();
    terminal
        .input(b"/bin/stty size; echo SIZE_\"TWO\"\n")
        .unwrap();
    let second = read_until(&mut terminal, "SIZE_TWO");
    terminal.stop();
    assert!(first.contains("40 120"), "{first}");
    assert!(second.contains("33 111"), "{second}");
    assert_eq!(
        announced,
        json!({"type":"pty_size", "pty_id":"size-test", "cols":111, "rows":33})
    );
}

//! The command advertised by clud.bot must be consumed by clud itself.
use crate::args::Args;

#[test]
fn advertised_remote_connect_is_not_backend_passthrough() {
    let args = Args::try_parse_from(["clud", "remote", "connect", "clud.bot"]);
    assert!(args.is_ok(), "advertised command must parse: {args:?}");
    let args = Args::parse_from_raw(
        ["clud", "remote", "connect", "clud.bot"]
            .map(str::to_owned)
            .to_vec(),
    );
    assert!(args.command.is_some());
    assert!(args.passthrough.is_empty());
}

#[test]
fn the_operator_hears_once_per_run_of_dropped_input() {
    use super::terminal::Delivery::{Dropped, Queued};
    let mut state = super::State::new(super::console::Console::with_sink(std::io::sink()));
    assert!(state.input_notice(Queued).is_none());
    let notice = state.input_notice(Dropped).expect("first drop is reported");
    assert_eq!(notice["code"], "input_dropped");
    assert!(state.input_notice(Dropped).is_none());
    assert!(state.input_notice(Queued).is_none());
    assert!(state.input_notice(Dropped).is_some());
}

/// On Windows, `is_terminal` on a piped stdin waits behind the stdin reader
/// thread's pending read, so the client hung before the consent question.
/// The terminal check must run once, before `stdin_lines` starts reading.
#[test]
fn stdin_terminal_check_happens_before_stdin_is_read() {
    let source = include_str!("mod.rs");
    let body = source.split("fn run_session(").nth(1).unwrap();
    let check = body
        .find("answers_inline()")
        .expect("checked in run_session");
    let reader = body.find("stdin_lines()").expect("reader started");
    assert!(
        check < reader,
        "check stdin before the reader thread starts"
    );
    assert_eq!(
        source.matches("answers_inline()").count(),
        2,
        "definition + one call"
    );
}

/// `sudo` is offered only where a session leader can share the ticket (not
/// Windows) and the password can be typed hidden (a terminal).
#[test]
fn sudo_is_advertised_only_on_a_terminal_off_windows() {
    let caps = |inline| super::caps(inline)["caps"].clone();
    assert_eq!(caps(false), serde_json::json!(["pty", "resume"]));
    if crate::platform::host::WINDOWS {
        assert_eq!(caps(true), serde_json::json!(["pty", "resume"]));
    } else {
        assert_eq!(caps(true), serde_json::json!(["pty", "resume", "sudo"]));
    }
}

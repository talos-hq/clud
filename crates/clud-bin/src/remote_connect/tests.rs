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

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

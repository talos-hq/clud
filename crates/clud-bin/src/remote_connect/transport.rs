//! Same-origin HTTP and WebSocket transport; bearer tokens never enter diagnostics.
use super::e2e;
use super::tls;
use serde::Deserialize;
use serde_json::Value;
use std::io::Read;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
use tungstenite::{Message, WebSocket};
use url::Url;

type Raw = WebSocket<tls::Stream>;

/// Hosts whose server always offers end-to-end encryption: a connection to
/// one that does not is refused, never downgraded to plaintext.
const ENCRYPTED_HOSTS: &[&str] = &["clud.bot"];
/// Requires encryption from any host (self-hosted servers, tests).
const REQUIRE_E2E: &str = "CLUD_REMOTE_REQUIRE_E2E";
/// How long creating the session and its first connection keep retrying a
/// network failure: one dropped packet on a poor link must not end the
/// command before it starts.
pub(super) const STARTUP_WINDOW: Duration = Duration::from_secs(60);
/// Shortens [`STARTUP_WINDOW`] (never lengthens it), for tests.
const STARTUP_WINDOW_ENV: &str = "CLUD_REMOTE_STARTUP_WINDOW_SECS";

/// The session socket. With end-to-end encryption every text and binary
/// message is sealed on send and opened on read, so callers see plaintext
/// messages either way.
///
/// After the handshake the socket is non-blocking. A send the network cannot
/// take yet stays queued in order (`backed_up`) and [`Socket::pump`] writes it
/// out later; a slow or briefly stalled link is waited out on this socket
/// rather than abandoned, because a reconnect loses whatever the old one had in
/// flight. Only silence ends a connection (the heartbeat, `resume.rs`). A
/// blocking send with a timeout is not an option: a write that times out on
/// Windows leaves the socket in an undefined state, and a TLS stream with a
/// torn record cannot continue.
pub(super) struct Socket {
    ws: Raw,
    channel: Option<e2e::Channel>,
    /// The first message of a server that did not offer encryption.
    early: Option<Message>,
    /// A record failed authentication; nothing more is read or sent.
    failed: bool,
    /// Sent messages are queued that the network has not taken yet.
    backlog: bool,
}

/// `Ok(false)` for a write the network could not take yet (it stays queued).
fn queued(result: tungstenite::Result<()>) -> tungstenite::Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(tungstenite::Error::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

impl Socket {
    fn plain(ws: Raw, early: Option<Message>) -> Self {
        Self {
            ws,
            channel: None,
            early,
            failed: false,
            backlog: false,
        }
    }

    /// Messages are waiting for the network; callers hold back bulk output
    /// until it drains, so the queue (and the delay of a heartbeat behind it)
    /// stays short.
    pub(super) fn backed_up(&self) -> bool {
        self.backlog
    }

    /// Write out what is queued, as far as the network takes it now.
    pub(super) fn pump(&mut self) -> tungstenite::Result<()> {
        if self.backlog && !self.failed {
            self.backlog = !queued(self.ws.flush())?;
        }
        Ok(())
    }

    pub(super) fn encrypted(&self) -> bool {
        self.channel.is_some()
    }

    pub(super) fn send(&mut self, message: Message) -> tungstenite::Result<()> {
        if self.failed {
            return Err(tungstenite::Error::AlreadyClosed);
        }
        let message = match (&mut self.channel, message) {
            (Some(channel), Message::Text(text)) => sealed(channel, e2e::TEXT, text.as_bytes())?,
            (Some(channel), Message::Binary(bytes)) => sealed(channel, e2e::BINARY, &bytes)?,
            (_, message) => message,
        };
        // tungstenite queues the frame before it writes: a write the network
        // cannot take yet keeps it, in order, for the next pump.
        self.backlog = !queued(self.ws.send(message))?;
        Ok(())
    }

    /// The next message, opened. A record that fails authentication (altered,
    /// replayed, reordered, truncated, or plaintext where a record belongs)
    /// closes the socket with 4400 and reads as that close.
    pub(super) fn read(&mut self) -> tungstenite::Result<Message> {
        if let Some(message) = self.early.take() {
            return Ok(message);
        }
        if self.failed {
            return Err(tungstenite::Error::AlreadyClosed);
        }
        let message = self.ws.read()?;
        let Some(channel) = &mut self.channel else {
            return Ok(message);
        };
        let opened = match &message {
            Message::Binary(record) => channel.open(record),
            Message::Text(_) | Message::Frame(_) => Err(()),
            Message::Ping(_) | Message::Pong(_) | Message::Close(_) => return Ok(message),
        };
        match opened {
            Ok((e2e::TEXT, payload)) => match String::from_utf8(payload) {
                Ok(text) => Ok(Message::Text(text.into())),
                Err(_) => Ok(self.fail()),
            },
            Ok((_, payload)) => Ok(Message::Binary(payload.into())),
            Err(()) => Ok(self.fail()),
        }
    }

    fn fail(&mut self) -> Message {
        self.failed = true;
        // Wipe the keys now; nothing more is accepted on this socket.
        self.channel = None;
        let frame = CloseFrame {
            code: CloseCode::from(e2e::FAILED_CLOSE),
            reason: "e2e".into(),
        };
        let _ = self.ws.close(Some(frame.clone()));
        let _ = self.ws.flush();
        Message::Close(Some(frame))
    }

    pub(super) fn close(&mut self, frame: Option<CloseFrame>) -> tungstenite::Result<()> {
        queued(self.ws.close(frame)).map(|_| ())
    }

    /// Close with `frame` and give a slow link up to `wait` to carry it (and
    /// what is queued before it), so the server ends the session at once
    /// instead of holding it for a reconnect that will not come.
    pub(super) fn close_within(&mut self, frame: CloseFrame, wait: Duration) {
        let deadline = Instant::now() + wait;
        if self.close(Some(frame)).is_err() {
            return;
        }
        while Instant::now() < deadline {
            match queued(self.ws.flush()) {
                Ok(false) => std::thread::sleep(Duration::from_millis(10)),
                Ok(true) | Err(_) => return,
            }
        }
    }
}

/// How long a session loop sleeps when a pass found nothing to do. The socket
/// is non-blocking, so this is the loop's only wait: short right after
/// activity (a keystroke's echo is not held back), doubling to a ceiling that
/// keeps an idle session cheap.
pub(super) struct Idle {
    wait: Duration,
}

impl Idle {
    pub(super) const FIRST: Duration = Duration::from_millis(1);
    pub(super) const CEILING: Duration = Duration::from_millis(20);

    pub(super) fn new() -> Self {
        Self { wait: Self::FIRST }
    }

    /// Something happened: the next idle pass waits the shortest time again.
    pub(super) fn active(&mut self) {
        self.wait = Self::FIRST;
    }

    /// The wait for this idle pass; the next one waits twice as long.
    pub(super) fn next(&mut self) -> Duration {
        let wait = self.wait;
        self.wait = (self.wait * 2).min(Self::CEILING);
        wait
    }

    /// Sleep for an idle pass, or reset after an active one.
    pub(super) fn after(&mut self, active: bool) {
        if active {
            self.active();
        } else {
            std::thread::sleep(self.next());
        }
    }
}

fn sealed(channel: &mut e2e::Channel, kind: u8, payload: &[u8]) -> tungstenite::Result<Message> {
    channel
        .seal(kind, payload)
        .map(|record| Message::Binary(record.into()))
        .map_err(|error| tungstenite::Error::Io(std::io::Error::other(error)))
}

#[derive(Clone, Deserialize)]
pub(super) struct Session {
    #[serde(default)]
    pub session_id: Option<String>,
    pub keyword: String,
    ws_url: String,
    pub client_token: String,
    #[serde(default = "default_proxy_model")]
    pub proxy_model: String,
    /// Echoed by a server that understands session modes.
    #[serde(default)]
    pub mode: Option<String>,
    /// The end-to-end encryption version the server says it offers.
    #[serde(default)]
    pub e2e: Option<u64>,
    /// Refuse a socket that is not end-to-end encrypted: the host always
    /// offers it, the server said it does, or an earlier socket was.
    #[serde(skip)]
    pub e2e_required: bool,
    /// The machine's name and directory were withheld from the creation
    /// request, to be sent inside the encrypted socket instead.
    #[serde(skip)]
    pub withheld: Option<Value>,
    /// A usable TLS peer learned by this session and shared with its reconnect worker.
    #[serde(skip)]
    last_peer: Arc<Mutex<Option<SocketAddr>>>,
}

fn default_proxy_model() -> String {
    "openrouter/auto".into()
}

pub(super) fn base_url(host: &str) -> Result<Url, String> {
    let text = if host.contains("://") {
        host.to_owned()
    } else {
        format!("https://{host}")
    };
    let base = Url::parse(&text).map_err(|_| "invalid remote host")?;
    let local = matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(base.scheme() == "https" || base.scheme() == "http" && local)
        || !base.username().is_empty()
        || base.password().is_some()
        || base.path() != "/"
        || base.query().is_some()
        || base.fragment().is_some()
        || base.host_str().is_none()
    {
        return Err("remote host must be an HTTPS origin (HTTP is allowed on loopback)".into());
    }
    Ok(base)
}

fn tls_config() -> Result<Arc<ureq::rustls::ClientConfig>, String> {
    // Preserve bundled public roots while allowing an operator-installed CA.
    // This is remote-only; unrelated ureq callers retain their existing trust.
    let mut roots = ureq::rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    let config = ureq::rustls::ClientConfig::builder_with_provider(
        ureq::rustls::crypto::ring::default_provider().into(),
    )
    .with_protocol_versions(&[&ureq::rustls::version::TLS12, &ureq::rustls::version::TLS13])
    .map_err(|_| "remote TLS protocol configuration failed")?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Whether `base` must be end-to-end encrypted before any server says so.
pub(super) fn expects_e2e(base: &Url, require: Option<&str>) -> bool {
    require == Some("1")
        || base
            .host_str()
            .is_some_and(|host| ENCRYPTED_HOSTS.contains(&host))
}

/// What the edge must not read: the computer's name, the user's home folder
/// name and the working directory. The operator tells machines apart by the
/// first two (#2899).
fn private_description() -> Value {
    let env = |key: &str| std::env::var(key).ok();
    serde_json::json!({
        "host": computer_name(
            sysinfo::System::host_name(),
            &[env("HOSTNAME"), env("COMPUTERNAME")],
        ),
        "user": home_folder_name(crate::home::user_home().as_deref()),
        "cwd": std::env::current_dir().ok().map(|p| p.display().to_string()),
    })
}

/// The OS's name for this computer. `HOSTNAME` is only a fallback: bash sets
/// it without exporting it, so a child process rarely sees it.
fn computer_name(from_os: Option<String>, from_env: &[Option<String>]) -> String {
    std::iter::once(from_os)
        .chain(from_env.iter().cloned())
        .flatten()
        .map(|name| name.trim().to_owned())
        .find(|name| !name.is_empty())
        .unwrap_or_default()
}

/// The last part of the user's home directory (`niteris` for `/home/niteris`).
fn home_folder_name(home: Option<&std::path::Path>) -> Option<String> {
    home?
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
}

/// Why one startup attempt failed.
enum Failure {
    /// The network or an overloaded server: worth another attempt.
    Retry(String),
    /// Another attempt would fail the same way.
    Fatal(String),
}

fn startup_window(value: Option<&str>) -> Duration {
    value
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map_or(STARTUP_WINDOW, |secs| {
            Duration::from_secs(secs).clamp(Duration::from_secs(1), STARTUP_WINDOW)
        })
}

/// Run `attempt` until it succeeds, fails for good, or the startup window
/// would pass before the next try. Waits use the reconnect backoff; the first
/// retry is announced once on stderr.
fn retrying<T>(host: &str, mut attempt: impl FnMut() -> Result<T, Failure>) -> Result<T, String> {
    let deadline =
        Instant::now() + startup_window(std::env::var(STARTUP_WINDOW_ENV).ok().as_deref());
    let mut backoff = super::resume::Backoff::default();
    let mut told = false;
    loop {
        let reason = match attempt() {
            Ok(value) => return Ok(value),
            Err(Failure::Fatal(reason)) => return Err(reason),
            Err(Failure::Retry(reason)) => reason,
        };
        let wait = backoff.next();
        if Instant::now() + wait >= deadline {
            return Err(reason);
        }
        if !std::mem::replace(&mut told, true) {
            eprintln!("remote: cannot reach {host} yet; retrying (Ctrl+C to stop)");
        }
        std::thread::sleep(wait);
    }
}

/// Whether a failed session creation is worth another attempt: the request
/// never got an answer, or the server is overloaded or restarting.
fn creation_failure(error: &ureq::Error) -> Failure {
    let retry = match error {
        ureq::Error::Status(code, _) => matches!(code, 408 | 429) || *code >= 500,
        ureq::Error::Transport(_) => true,
    };
    let reason = "could not create remote session".to_owned();
    if retry {
        Failure::Retry(reason)
    } else {
        Failure::Fatal(reason)
    }
}

/// Create the session, retrying network failures within [`STARTUP_WINDOW`].
/// No keyword exists before this succeeds, so a retry creates nothing the
/// user has seen; a request answered after its timeout leaves a session the
/// server expires unused.
pub(super) fn create(base: &Url, mode: Option<&str>) -> Result<Session, String> {
    let host = base.host_str().unwrap_or("the remote server").to_owned();
    retrying(&host, || create_once(base, mode))
}

fn create_once(base: &Url, mode: Option<&str>) -> Result<Session, Failure> {
    let fatal = |reason: &str| Failure::Fatal(reason.into());
    let expected = expects_e2e(base, std::env::var(REQUIRE_E2E).ok().as_deref());
    let private = private_description();
    let mut client = serde_json::json!({
        "os": std::env::consts::OS,
        "clud_version": env!("CARGO_PKG_VERSION"),
    });
    // Sent as `client_info` inside the encrypted socket when it will be one.
    if !expected {
        for (key, value) in private.as_object().into_iter().flatten() {
            client[key] = value.clone();
        }
    }
    let mut body = serde_json::json!({ "client": client });
    if let Some(mode) = mode {
        body["mode"] = mode.into();
    }
    let response = ureq::AgentBuilder::new()
        .tls_config(tls_config().map_err(Failure::Fatal)?)
        .redirects(0)
        .timeout(Duration::from_secs(15))
        .build()
        .post(base.join("api/remote/sessions").unwrap().as_str())
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|error| creation_failure(&error))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(16385)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::Retry("could not read remote session".into()))?;
    if bytes.len() > 16384 {
        return Err(fatal("oversized remote session response"));
    }
    let mut session: Session =
        serde_json::from_slice(&bytes).map_err(|_| fatal("invalid remote session response"))?;
    session.e2e_required = expected || session.e2e.is_some();
    session.withheld = expected.then_some(private);
    if session.keyword.len() != 11
        || session.keyword.as_bytes()[5] != b'-'
        || !session
            .keyword
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 5 || b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(&b))
    {
        return Err(fatal("invalid remote keyword"));
    }
    validate_socket_url(base, &session.ws_url).map_err(Failure::Fatal)?;
    let ws = Url::parse(&session.ws_url).map_err(|_| fatal("invalid remote WebSocket URL"))?;
    if ws
        .query_pairs()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
        .as_deref()
        != Some(session.client_token.as_str())
    {
        return Err(fatal("remote session bearer does not match its socket"));
    }
    Ok(session)
}

fn validate_socket_url(base: &Url, text: &str) -> Result<(), String> {
    let ws = Url::parse(text).map_err(|_| "invalid remote WebSocket URL")?;
    let scheme = if base.scheme() == "https" {
        "wss"
    } else {
        "ws"
    };
    if ws.scheme() != scheme
        || ws.host_str() != base.host_str()
        || ws.port_or_known_default() != base.port_or_known_default()
        || !ws.username().is_empty()
        || ws.password().is_some()
        || ws.path() != "/ws/client"
    {
        return Err("remote WebSocket must use the session server's origin".into());
    }
    Ok(())
}

/// Why a WebSocket connection was not made.
#[derive(Debug)]
pub(super) enum ConnectError {
    /// The server answered the handshake with an HTTP error: the token is no
    /// longer accepted (the session ended) or the request was refused.
    Refused,
    /// The server could not be reached or the handshake did not complete.
    Unreachable(String),
    /// The socket would not be end-to-end encrypted as required, or the key
    /// exchange failed. Final: retrying cannot make it safe.
    Insecure(String),
}

impl From<ConnectError> for String {
    fn from(error: ConnectError) -> Self {
        match error {
            ConnectError::Refused => "could not connect remote session".into(),
            ConnectError::Unreachable(reason) | ConnectError::Insecure(reason) => reason,
        }
    }
}

/// The session's socket URL, declaring resume support (`resume=1`): a server
/// that understands it keeps the session through a lost socket, and lets a
/// newer connection with this token replace an older one. `e2e=1` asks for an
/// encrypted socket. Servers ignore what they do not understand.
fn socket_url(session: &Session) -> String {
    format!("{}&resume=1&e2e={}", session.ws_url, e2e::VERSION)
}

/// Connect the session's socket. After it, `session` requires encryption on
/// every later socket if this one was encrypted; a plaintext socket the user
/// was not warned about yet is announced on stderr.
pub(super) fn connect(session: &mut Session) -> Result<Socket, String> {
    let host = Url::parse(&session.ws_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "the remote server".into());
    // The same token every time; an unreachable server is retried, a refusal
    // or an unsafe socket is not.
    let mut socket = retrying(&host, || match connect_socket(session) {
        Ok(socket) => Ok(socket),
        Err(ConnectError::Unreachable(reason)) => Err(Failure::Retry(reason)),
        Err(error) => Err(Failure::Fatal(String::from(error))),
    })?;
    if socket.encrypted() {
        session.e2e_required = true;
        if let Some(private) = session.withheld.take() {
            let info = serde_json::json!({"type":"client_info", "client":private});
            socket
                .send(Message::Text(info.to_string().into()))
                .map_err(|_| "remote connection lost")?;
        }
    } else {
        eprintln!(
            "remote: this server does not offer end-to-end encryption; \
             anything between you and it that terminates TLS can read this session."
        );
    }
    Ok(socket)
}

/// Run the key exchange if the server offers it (its first message), or keep
/// the first message of a server that does not, unless `session` requires it.
fn secure(mut ws: Raw, session: &Session) -> Result<Socket, ConnectError> {
    let insecure = |reason: &str| ConnectError::Insecure(reason.into());
    let first = ws
        .read()
        .map_err(|_| ConnectError::Unreachable("remote server did not greet".into()))?;
    let event: Value = match &first {
        Message::Text(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let Some(offer) = e2e::parse_offer(&event).map_err(|error| insecure(&error))? else {
        if session.e2e_required {
            // Deliberate (1000): the server ends this session; it was not resumable.
            let _ = ws.close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "".into(),
            }));
            let _ = ws.flush();
            return Err(insecure(e2e::DOWNGRADE));
        }
        return Ok(Socket::plain(ws, Some(first)));
    };
    let session_id = session
        .session_id
        .as_deref()
        .ok_or_else(|| insecure("clud.bot offered encryption without a session id"))?;
    let handshake = e2e::Handshake::new().map_err(|error| insecure(&error))?;
    ws.send(Message::Text(handshake.accept().to_string().into()))
        .map_err(|_| ConnectError::Unreachable("remote connection lost".into()))?;
    let channel = handshake
        .finish(session_id, &offer)
        .map_err(|error| insecure(&error))?;
    Ok(Socket {
        ws,
        channel: Some(channel),
        early: None,
        failed: false,
        backlog: false,
    })
}

fn dial_addresses(
    resolved: std::io::Result<Vec<SocketAddr>>,
    last_peer: Option<SocketAddr>,
) -> Result<Vec<SocketAddr>, ConnectError> {
    const MAX_ADDRESSES: usize = 4;
    let mut addresses = Vec::new();
    for address in resolved.unwrap_or_default() {
        if !addresses.contains(&address) {
            addresses.push(address);
            if addresses.len() == MAX_ADDRESSES {
                break;
            }
        }
    }
    if let Some(peer) = last_peer.filter(|peer| !addresses.contains(peer)) {
        if addresses.len() == MAX_ADDRESSES {
            addresses.pop();
        }
        addresses.push(peer);
    }
    if addresses.is_empty() {
        return Err(ConnectError::Unreachable(
            "cannot resolve remote host".into(),
        ));
    }
    Ok(addresses)
}

pub(super) fn connect_socket(session: &Session) -> Result<Socket, ConnectError> {
    connect_socket_with(
        session,
        |host, port| (host, port).to_socket_addrs().map(Iterator::collect),
        tls_config,
    )
}

pub(super) fn connect_socket_with(
    session: &Session,
    resolve: impl FnOnce(&str, u16) -> std::io::Result<Vec<SocketAddr>>,
    tls_config: impl FnOnce() -> Result<Arc<ureq::rustls::ClientConfig>, String>,
) -> Result<Socket, ConnectError> {
    let ws_url = socket_url(session);
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(128 * 1024))
        .max_frame_size(Some(128 * 1024));
    let unreachable = |reason: &str| ConnectError::Unreachable(reason.into());
    let url = Url::parse(&ws_url).map_err(|_| unreachable("invalid remote WebSocket URL"))?;
    let host = url
        .host_str()
        .ok_or_else(|| unreachable("missing remote host"))?
        .trim_matches(['[', ']']);
    let peer = *session
        .last_peer
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let port = url
        .port_or_known_default()
        .ok_or_else(|| unreachable("missing remote port"))?;
    let resolved = resolve(host, port);
    let timeout = Duration::from_secs(15);
    let (tcp, address) = dial_addresses(resolved, peer)?
        .into_iter()
        .find_map(|address| {
            TcpStream::connect_timeout(&address, timeout)
                .ok()
                .map(|tcp| (tcp, address))
        })
        .ok_or_else(|| unreachable("cannot reach remote host"))?;
    tcp.set_read_timeout(Some(timeout))
        .map_err(|_| unreachable("cannot set handshake timeout"))?;
    tcp.set_write_timeout(Some(timeout))
        .map_err(|_| unreachable("cannot set handshake timeout"))?;
    // TLS through `tls::TlsStream`, which stays sound once the socket is
    // non-blocking (rustls's own stream does not; see `tls.rs`).
    let stream = if url.scheme() == "wss" {
        let config = tls_config().map_err(ConnectError::Unreachable)?;
        tls::Stream::Tls(Box::new(
            tls::TlsStream::new(config, host, tcp)
                .map_err(|_| unreachable("invalid remote host name"))?,
        ))
    } else {
        tls::Stream::Plain(tcp)
    };
    // One handshake; redirects are never followed.
    let (socket, _) = tungstenite::client::client_with_config(
        ws_url.as_str(),
        stream,
        Some(config),
    )
    .map_err(|error| match error {
        tungstenite::HandshakeError::Failure(tungstenite::Error::Http(_)) => ConnectError::Refused,
        _ => unreachable("could not connect remote session"),
    })?;
    // The exchange runs under the handshake's timeouts.
    let socket = secure(socket, session)?;
    let tcp = socket.ws.get_ref().tcp();
    // Non-blocking from here on (see `Socket`); the session loop paces itself.
    tcp.set_nonblocking(true)
        .map_err(|_| unreachable("cannot make the remote socket non-blocking"))?;
    // Keystrokes and their echo are small: send them now, not after an ACK.
    let _ = tcp.set_nodelay(true);
    if url.scheme() == "wss" {
        *session
            .last_peer
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(address);
    }
    Ok(socket)
}

pub(super) fn terminal_status(base: &Url, keyword: &str) -> Option<String> {
    let response = ureq::AgentBuilder::new()
        .tls_config(tls_config().ok()?)
        .redirects(0)
        .timeout(Duration::from_secs(15))
        .build()
        .get(base.join("api/remote/status").ok()?.as_str())
        .query("keyword", keyword)
        .call()
        .ok()?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(4097)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value["state"].as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_secure_same_origin_transport() {
        let base = base_url("clud.bot").unwrap();
        assert_eq!(base.as_str(), "https://clud.bot/");
        assert!(base_url("http://clud.bot").is_err());
        assert!(base_url("https://user:password@clud.bot").is_err());
        assert!(base_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_socket_url(&base, "wss://other.example/ws/client?token=secret").is_err());
        assert!(validate_socket_url(&base, "ws://clud.bot/ws/client?token=secret").is_err());
        assert!(validate_socket_url(&base, "wss://clud.bot/ws/client?token=secret").is_ok());
    }

    #[test]
    fn clud_bot_always_requires_encryption_and_others_may_opt_in() {
        let bot = base_url("clud.bot").unwrap();
        assert!(expects_e2e(&bot, None));
        assert!(expects_e2e(
            &base_url("https://clud.bot").unwrap(),
            Some("0")
        ));
        let other = base_url("https://example.com").unwrap();
        assert!(!expects_e2e(&other, None));
        assert!(!expects_e2e(&other, Some("0")));
        assert!(expects_e2e(&other, Some("1")));
        // A look-alike host is not clud.bot.
        assert!(!expects_e2e(
            &base_url("https://clud.bot.example").unwrap(),
            None
        ));
    }

    #[test]
    fn the_startup_window_is_a_minute_and_can_only_be_shortened() {
        assert_eq!(startup_window(None), STARTUP_WINDOW);
        assert_eq!(startup_window(Some("5")), Duration::from_secs(5));
        assert_eq!(startup_window(Some("0")), Duration::from_secs(1));
        assert_eq!(startup_window(Some("3600")), STARTUP_WINDOW);
        assert_eq!(startup_window(Some("later")), STARTUP_WINDOW);
    }

    #[test]
    fn only_a_network_failure_or_an_overloaded_server_is_retried() {
        let status = |code| {
            let response = ureq::Response::new(code, "status", "").unwrap();
            ureq::Error::Status(code, response)
        };
        for code in [408, 429, 500, 502, 503] {
            assert!(
                matches!(creation_failure(&status(code)), Failure::Retry(_)),
                "{code}"
            );
        }
        for code in [400, 401, 403, 404, 422] {
            assert!(
                matches!(creation_failure(&status(code)), Failure::Fatal(_)),
                "{code}"
            );
        }
    }

    #[test]
    fn retrying_stops_at_success_a_fatal_failure_or_the_window() {
        let mut calls = 0;
        let value = retrying("h", || {
            calls += 1;
            if calls < 3 {
                Err(Failure::Retry("down".into()))
            } else {
                Ok(7)
            }
        });
        assert_eq!((value, calls), (Ok(7), 3));
        let mut calls = 0;
        let fatal: Result<(), _> = retrying("h", || {
            calls += 1;
            Err(Failure::Fatal("refused".into()))
        });
        assert_eq!((fatal, calls), (Err("refused".into()), 1));
    }

    #[test]
    fn an_idle_loop_backs_off_to_its_ceiling_and_wakes_fast_after_activity() {
        let mut idle = Idle::new();
        assert_eq!(idle.next(), Idle::FIRST);
        assert_eq!(idle.next(), Idle::FIRST * 2);
        for _ in 0..10 {
            assert!(idle.next() <= Idle::CEILING);
        }
        assert_eq!(idle.next(), Idle::CEILING);
        idle.active();
        assert_eq!(idle.next(), Idle::FIRST);
    }

    #[test]
    fn a_write_the_network_cannot_take_yet_is_queued_not_failed() {
        let blocked = std::io::Error::from(std::io::ErrorKind::WouldBlock);
        assert!(!queued(Err(tungstenite::Error::Io(blocked))).unwrap());
        assert!(queued(Ok(())).unwrap());
        let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert!(queued(Err(tungstenite::Error::Io(reset))).is_err());
        // A timeout is not a would-block: only a non-blocking socket is retried.
        let timed_out = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert!(queued(Err(tungstenite::Error::Io(timed_out))).is_err());
    }

    #[test]
    fn reconnect_uses_a_previous_address_when_dns_fails_or_returns_no_records() {
        let previous = "192.0.2.42:443".parse().unwrap();
        let fresh = "[2001:db8::1]:443".parse().unwrap();
        let failure = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            dial_addresses(Err(failure), Some(previous)).unwrap(),
            [previous]
        );
        assert_eq!(
            dial_addresses(Ok(vec![]), Some(previous)).unwrap(),
            [previous]
        );
        assert_eq!(
            dial_addresses(Ok(vec![fresh, previous]), Some(previous)).unwrap(),
            [fresh, previous]
        );
        assert!(dial_addresses(Ok(vec![]), None).is_err());
    }

    #[test]
    fn the_socket_url_asks_for_resume_and_encryption() {
        let session: Session = serde_json::from_value(serde_json::json!({
            "keyword": "ABCDE-FGH23",
            "ws_url": "wss://clud.bot/ws/client?token=t",
            "client_token": "t",
        }))
        .unwrap();
        assert_eq!(
            socket_url(&session),
            "wss://clud.bot/ws/client?token=t&resume=1&e2e=1"
        );
        // An old server's response: nothing says it offers encryption.
        assert!(!session.e2e_required && session.e2e.is_none());
    }

    #[test]
    fn the_computer_name_prefers_the_os_and_skips_blank_fallbacks() {
        let env = |a: Option<&str>, b: Option<&str>| [a.map(Into::into), b.map(Into::into)];
        assert_eq!(
            computer_name(Some("box".into()), &env(Some("x"), None)),
            "box"
        );
        // bash's unexported HOSTNAME is usually absent; COMPUTERNAME is Windows.
        assert_eq!(computer_name(None, &env(None, Some("WIN-PC"))), "WIN-PC");
        assert_eq!(
            computer_name(Some("  ".into()), &env(Some(""), Some("pc"))),
            "pc"
        );
        assert_eq!(computer_name(None, &env(None, None)), "");
    }

    #[test]
    fn the_user_is_the_home_folder_name() {
        use std::path::Path;
        assert_eq!(
            home_folder_name(Some(Path::new("/home/niteris"))).as_deref(),
            Some("niteris")
        );
        assert_eq!(home_folder_name(Some(Path::new("/"))), None);
        assert_eq!(home_folder_name(None), None);
    }

    #[test]
    fn the_private_description_names_this_computer() {
        let private = private_description();
        assert!(
            !private["host"].as_str().unwrap_or_default().is_empty(),
            "{private}"
        );
        assert!(
            private.get("user").is_some() && private.get("cwd").is_some(),
            "{private}"
        );
    }
}

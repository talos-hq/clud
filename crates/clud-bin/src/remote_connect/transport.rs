//! Same-origin HTTP and WebSocket transport; bearer tokens never enter diagnostics.
use super::e2e;
use serde::Deserialize;
use serde_json::Value;
use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};
use url::Url;

type Raw = WebSocket<MaybeTlsStream<TcpStream>>;

/// Hosts whose server always offers end-to-end encryption: a connection to
/// one that does not is refused, never downgraded to plaintext.
const ENCRYPTED_HOSTS: &[&str] = &["clud.bot"];
/// Requires encryption from any host (self-hosted servers, tests).
const REQUIRE_E2E: &str = "CLUD_REMOTE_REQUIRE_E2E";

/// The session socket. With end-to-end encryption every text and binary
/// message is sealed on send and opened on read, so callers see plaintext
/// messages either way.
pub(super) struct Socket {
    ws: Raw,
    channel: Option<e2e::Channel>,
    /// The first message of a server that did not offer encryption.
    early: Option<Message>,
    /// A record failed authentication; nothing more is read or sent.
    failed: bool,
}

impl Socket {
    fn plain(ws: Raw, early: Option<Message>) -> Self {
        Self {
            ws,
            channel: None,
            early,
            failed: false,
        }
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
        self.ws.send(message)
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
        self.ws.close(frame)
    }

    pub(super) fn flush(&mut self) -> tungstenite::Result<()> {
        self.ws.flush()
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
    pub ws_url: String,
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

/// What the edge must not read: the machine's name and working directory.
fn private_description() -> Value {
    serde_json::json!({
        "host": std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).unwrap_or_default(),
        "cwd": std::env::current_dir().ok().map(|p| p.display().to_string()),
    })
}

pub(super) fn create(base: &Url, mode: Option<&str>) -> Result<Session, String> {
    let expected = expects_e2e(base, std::env::var(REQUIRE_E2E).ok().as_deref());
    let private = private_description();
    let mut client = serde_json::json!({
        "os": std::env::consts::OS,
        "clud_version": env!("CARGO_PKG_VERSION"),
    });
    // Sent as `client_info` inside the encrypted socket when it will be one.
    if !expected {
        client["host"] = private["host"].clone();
        client["cwd"] = private["cwd"].clone();
    }
    let mut body = serde_json::json!({ "client": client });
    if let Some(mode) = mode {
        body["mode"] = mode.into();
    }
    let response = ureq::AgentBuilder::new()
        .tls_config(tls_config()?)
        .redirects(0)
        .timeout(Duration::from_secs(15))
        .build()
        .post(base.join("api/remote/sessions").unwrap().as_str())
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|_| "could not create remote session")?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(16385)
        .read_to_end(&mut bytes)
        .map_err(|_| "could not read remote session")?;
    if bytes.len() > 16384 {
        return Err("oversized remote session response".into());
    }
    let mut session: Session =
        serde_json::from_slice(&bytes).map_err(|_| "invalid remote session response")?;
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
        return Err("invalid remote keyword".into());
    }
    validate_socket_url(base, &session.ws_url)?;
    let ws = Url::parse(&session.ws_url).map_err(|_| "invalid remote WebSocket URL")?;
    if ws
        .query_pairs()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
        .as_deref()
        != Some(session.client_token.as_str())
    {
        return Err("remote session bearer does not match its socket".into());
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
pub(super) fn socket_url(session: &Session) -> String {
    format!("{}&resume=1&e2e={}", session.ws_url, e2e::VERSION)
}

/// Connect the session's socket. After it, `session` requires encryption on
/// every later socket if this one was encrypted; a plaintext socket the user
/// was not warned about yet is announced on stderr.
pub(super) fn connect(session: &mut Session) -> Result<Socket, String> {
    let mut socket = connect_url(&socket_url(session), session).map_err(String::from)?;
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
    })
}

pub(super) fn connect_url(ws_url: &str, session: &Session) -> Result<Socket, ConnectError> {
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(128 * 1024))
        .max_frame_size(Some(128 * 1024));
    let unreachable = |reason: &str| ConnectError::Unreachable(reason.into());
    let url = Url::parse(ws_url).map_err(|_| unreachable("invalid remote WebSocket URL"))?;
    let host = url
        .host_str()
        .ok_or_else(|| unreachable("missing remote host"))?
        .trim_matches(['[', ']']);
    let addresses = (
        host,
        url.port_or_known_default()
            .ok_or_else(|| unreachable("missing remote port"))?,
    )
        .to_socket_addrs()
        .map_err(|_| unreachable("cannot resolve remote host"))?;
    let timeout = Duration::from_secs(15);
    let tcp = addresses
        .take(4)
        .find_map(|address| TcpStream::connect_timeout(&address, timeout).ok())
        .ok_or_else(|| unreachable("cannot reach remote host"))?;
    tcp.set_read_timeout(Some(timeout))
        .map_err(|_| unreachable("cannot set handshake timeout"))?;
    tcp.set_write_timeout(Some(timeout))
        .map_err(|_| unreachable("cannot set handshake timeout"))?;
    // Unlike connect(), this performs one handshake and never follows redirects.
    let (socket, _) = tungstenite::client_tls_with_config(
        ws_url,
        tcp,
        Some(config),
        Some(tungstenite::Connector::Rustls(
            tls_config().map_err(ConnectError::Unreachable)?,
        )),
    )
    .map_err(|error| match error {
        tungstenite::HandshakeError::Failure(tungstenite::Error::Http(_)) => ConnectError::Refused,
        _ => unreachable("could not connect remote session"),
    })?;
    // The exchange runs under the handshake's timeouts.
    let mut socket = secure(socket, session)?;
    let tcp = match socket.ws.get_mut() {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => &mut tls.sock,
        _ => return Err(unreachable("unsupported remote transport")),
    };
    tcp.set_read_timeout(Some(Duration::from_millis(50)))
        .map_err(|_| unreachable("cannot set remote read timeout"))?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| unreachable("cannot set remote write timeout"))?;
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
}

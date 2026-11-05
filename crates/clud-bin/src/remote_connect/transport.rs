//! Same-origin HTTP and WebSocket transport; bearer tokens never enter diagnostics.
use serde::Deserialize;
use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;
use tungstenite::{stream::MaybeTlsStream, WebSocket};
use url::Url;

pub(super) type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

#[derive(Deserialize)]
pub(super) struct Session {
    pub keyword: String,
    pub ws_url: String,
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

pub(super) fn create(base: &Url) -> Result<Session, String> {
    let body = serde_json::json!({"client": {
        "host": std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).unwrap_or_default(),
        "os": std::env::consts::OS,
        "clud_version": env!("CARGO_PKG_VERSION"),
        "cwd": std::env::current_dir().ok().map(|p| p.display().to_string()),
    }});
    let response = ureq::AgentBuilder::new()
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
    let session: Session =
        serde_json::from_slice(&bytes).map_err(|_| "invalid remote session response")?;
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

pub(super) fn connect(session: &Session) -> Result<Socket, String> {
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(128 * 1024))
        .max_frame_size(Some(128 * 1024));
    let url = Url::parse(&session.ws_url).map_err(|_| "invalid remote WebSocket URL")?;
    let host = url
        .host_str()
        .ok_or("missing remote host")?
        .trim_matches(['[', ']']);
    let addresses = (
        host,
        url.port_or_known_default().ok_or("missing remote port")?,
    )
        .to_socket_addrs()
        .map_err(|_| "cannot resolve remote host")?;
    let timeout = Duration::from_secs(15);
    let tcp = addresses
        .take(4)
        .find_map(|address| TcpStream::connect_timeout(&address, timeout).ok())
        .ok_or("cannot reach remote host")?;
    tcp.set_read_timeout(Some(timeout))
        .map_err(|_| "cannot set handshake timeout")?;
    tcp.set_write_timeout(Some(timeout))
        .map_err(|_| "cannot set handshake timeout")?;
    // Unlike connect(), this performs one handshake and never follows redirects.
    let (mut socket, _) =
        tungstenite::client_tls_with_config(session.ws_url.as_str(), tcp, Some(config), None)
            .map_err(|_| "could not connect remote session")?;
    let tcp = match socket.get_mut() {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => &mut tls.sock,
        _ => return Err("unsupported remote transport".into()),
    };
    tcp.set_read_timeout(Some(Duration::from_millis(50)))
        .map_err(|_| "cannot set remote read timeout")?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| "cannot set remote write timeout")?;
    Ok(socket)
}

pub(super) fn terminal_status(base: &Url, keyword: &str) -> Option<String> {
    let response = ureq::AgentBuilder::new()
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
}

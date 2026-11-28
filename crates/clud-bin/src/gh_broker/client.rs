//! The shim half of the read broker (#1743).
//!
//! [`BrokerClient::read`] asks the daemon's loopback HTTP listener for one
//! endpoint. [`serve_replay`] then serves that response once from a
//! loopback port on a random path, and the shim reruns the caller's own
//! `gh api` argv against that URL. Formatting (`--jq`, `--template`,
//! `--silent`, TTY pretty-printing, colors) therefore stays the real `gh`'s
//! own code over the same body bytes, which is what makes the output
//! byte-identical by construction. Go never proxies loopback, and the only
//! credential `gh` could attach for the loopback host (an enterprise token)
//! goes to this process's own listener.
//!
//! Every miss returns `None` and the shim runs the real `gh` unchanged:
//! no daemon, an old daemon without the route, a timeout, a non-2xx
//! upstream status.

use std::io::{IsTerminal, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;

use super::classify::ApiRead;
use super::{ReadReply, ReadRequest, StaleNote, INVALIDATE_PATH, READ_PATH};

/// Every stale mark the shim prints starts with this, so a script that
/// must not act on a cached copy (a merge gate) can tell from stderr.
pub const STALE_MARKER: &str = "clud: gh read broker: rate-limit reserve";

/// The stderr line for a stored copy served below the rate-limit floor.
pub fn stale_marker(note: &StaleNote) -> String {
    let reset = super::collection::format_ts(i64::try_from(note.reset_s).unwrap_or(i64::MAX));
    format!(
        "{STALE_MARKER} ({} of {} requests left until {reset}): a cached copy from {}s ago, not a refresh",
        note.remaining,
        note.limit,
        note.age_ms / 1000
    )
}

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// Covers the daemon's own 30 s upstream timeout plus a queue of readers.
const READ_TIMEOUT: Duration = Duration::from_secs(45);
const INVALIDATE_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_REPLY_BYTES: u64 = 48 * 1024 * 1024;

/// Everything the shim needs, resolved by dispatch from the session env.
#[derive(Debug, Clone)]
pub struct BrokerClient {
    /// The daemon state dir whose `daemon.json` names the listener.
    pub state_dir: PathBuf,
    /// The session's real `gh`.
    pub gh: PathBuf,
    /// [`super::FORWARDED_ENV`] values the caller had set.
    pub env: Vec<(String, String)>,
    pub session_id: Option<String>,
    pub fresh: bool,
    /// The caller's `GH_REPO`: the repo a write without `-R` targets, for
    /// targeted invalidation.
    pub gh_repo: Option<String>,
}

/// A response to replay to the real `gh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replayable {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Deserialize)]
struct DaemonListener {
    dashboard_port: Option<u16>,
    dashboard_token: Option<String>,
}

impl BrokerClient {
    fn listener(&self) -> Option<(String, String)> {
        let text = std::fs::read_to_string(self.state_dir.join("daemon.json")).ok()?;
        let info: DaemonListener = serde_json::from_str(&text).ok()?;
        let port = info.dashboard_port?;
        let token = info.dashboard_token.filter(|t| !t.is_empty())?;
        Some((format!("127.0.0.1:{port}"), token))
    }

    fn post(&self, path: &str, body: &[u8], timeout: Duration) -> Option<ureq::Response> {
        let (host, token) = self.listener()?;
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout(timeout)
            .build()
            .post(&format!("http://{host}{path}"))
            .set("Content-Type", "application/json")
            .set("Host", &host)
            .set("Cookie", &format!("clud_dashboard_token={token}"))
            .send_bytes(body)
            .ok()
    }

    /// The broker's response for `read`, or `None` to run the real `gh`.
    pub fn read(&self, read: &ApiRead) -> Option<Replayable> {
        let request = ReadRequest {
            gh: self.gh.to_str()?.to_string(),
            endpoint: read.endpoint.clone(),
            hostname: read.hostname.clone(),
            env: self.env.clone(),
            session_id: self.session_id.clone(),
            fresh: self.fresh,
            interactive: std::io::stdin().is_terminal(),
        };
        let body = serde_json::to_vec(&request).ok()?;
        let response = self.post(READ_PATH, &body, READ_TIMEOUT)?;
        if response.status() != 200 {
            return None;
        }
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(MAX_REPLY_BYTES)
            .read_to_end(&mut bytes)
            .ok()?;
        let reply: ReadReply = serde_json::from_slice(&bytes).ok()?;
        if !(200..300).contains(&reply.status) {
            return None;
        }
        let body = base64::engine::general_purpose::STANDARD
            .decode(reply.body_b64)
            .ok()?;
        if let Some(note) = &reply.stale {
            eprintln!("{}", stale_marker(note));
        }
        Some(Replayable {
            status: reply.status,
            headers: reply.headers,
            body,
        })
    }

    /// Best effort: tell the broker GitHub state may have changed. `tags`
    /// names what changed ([`super::scope::write_tags`]); `None` means it
    /// could be anything.
    pub fn invalidate(&self, tags: Option<&[String]>) {
        let body = match tags {
            Some(tags) => serde_json::json!({ "tags": tags }),
            None => serde_json::json!({}),
        };
        let _ = self.post(
            INVALIDATE_PATH,
            body.to_string().as_bytes(),
            INVALIDATE_TIMEOUT,
        );
    }
}

/// Headers the replay must not copy: they describe the upstream transfer,
/// not this one (`gh` already decompressed the body).
fn replayed_header(name: &str) -> bool {
    ![
        "content-length",
        "content-encoding",
        "transfer-encoding",
        "connection",
        "keep-alive",
    ]
    .iter()
    .any(|skip| name.eq_ignore_ascii_case(skip))
}

/// Render the HTTP/1.1 response the replay server sends.
pub fn render_response(response: &Replayable) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {} OK\r\n", response.status).into_bytes();
    for (name, value) in &response.headers {
        if replayed_header(name) && !name.contains(['\r', '\n']) && !value.contains(['\r', '\n']) {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
    }
    out.extend_from_slice(
        format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            response.body.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&response.body);
    out
}

/// Serve `response` on a loopback port under a random path until this
/// process exits, and return the URL. Other paths get a 404, so another
/// local process cannot read the body without the path.
pub fn serve_replay(response: Replayable) -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let path = format!(
        "/clud-gh-replay/{}",
        crate::dashboard_auth::generate_token()
    );
    let url = format!("http://127.0.0.1:{port}{path}");
    let rendered = Arc::new(render_response(&response));
    std::thread::Builder::new()
        .name("clud-gh-replay".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let rendered = Arc::clone(&rendered);
                let path = path.clone();
                let _ = std::thread::Builder::new()
                    .name("clud-gh-replay-conn".into())
                    .spawn(move || serve_one(stream, &path, &rendered));
            }
        })?;
    Ok(url)
}

fn serve_one(mut stream: TcpStream, path: &str, rendered: &[u8]) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 64 * 1024 {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let line = head.split(|&b| b == b'\n').next().unwrap_or_default();
    let line = String::from_utf8_lossy(line);
    let mut parts = line.split_whitespace();
    let matches = parts.next() == Some("GET") && parts.next() == Some(path);
    let reply: &[u8] = if matches {
        rendered
    } else {
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    };
    let _ = stream.write_all(reply);
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Err` carries the HTTP status (0 for a transport error).
    fn get(url: &str) -> Result<(u16, Vec<u8>, Option<String>), u16> {
        let response = ureq::get(url).call().map_err(|error| match error {
            ureq::Error::Status(code, _) => code,
            ureq::Error::Transport(_) => 0,
        })?;
        let status = response.status();
        let content_type = response.header("content-type").map(str::to_string);
        let mut body = Vec::new();
        response.into_reader().read_to_end(&mut body).unwrap();
        Ok((status, body, content_type))
    }

    #[test]
    fn replay_serves_the_body_bytes_exactly_on_its_path_only() {
        let body = b"{\"a\":\"\\u00e9\"}\r\n\xff no trailing newline".to_vec();
        let url = serve_replay(Replayable {
            status: 200,
            headers: vec![
                (
                    "Content-Type".into(),
                    "application/json; charset=utf-8".into(),
                ),
                ("Content-Length".into(), "999".into()),
                ("Content-Encoding".into(), "gzip".into()),
            ],
            body: body.clone(),
        })
        .unwrap();
        let (status, got, content_type) = get(&url).unwrap();
        assert_eq!(status, 200);
        assert_eq!(got, body);
        assert_eq!(
            content_type.as_deref(),
            Some("application/json; charset=utf-8")
        );
        // Served for every request on the path, never on another one.
        assert_eq!(get(&url).unwrap().1, body);
        let other = url.rsplit_once('/').unwrap().0.to_string() + "/guess";
        assert_eq!(get(&other), Err(404));
    }

    #[test]
    fn a_missing_daemon_is_a_miss_not_an_error() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let client = BrokerClient {
            state_dir: dir.path().to_path_buf(),
            gh: PathBuf::from("/usr/bin/gh"),
            env: vec![],
            session_id: None,
            fresh: false,
            gh_repo: None,
        };
        let read = super::super::classify::api_read(&["api".into(), "repos/o/r".into()]).unwrap();
        assert_eq!(client.read(&read), None);
        // A daemon.json naming a dead port is a miss too, and fast.
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);
        std::fs::write(
            dir.path().join("daemon.json"),
            format!(r#"{{"dashboard_port":{port},"dashboard_token":"t"}}"#),
        )
        .unwrap();
        let started = std::time::Instant::now();
        assert_eq!(client.read(&read), None);
        assert!(started.elapsed() < Duration::from_secs(5));
        client.invalidate(None);
    }
}

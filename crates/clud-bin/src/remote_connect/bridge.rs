//! Loopback endpoint that carries the agent's model requests over the session
//! socket (server channel 2) instead of separate HTTPS requests.
//!
//! The hosting edge in front of clud.bot runs a request firewall over HTTPS
//! bodies. An agent's prompt carries shell commands and tool output, which it
//! answers with an HTML `403 Blocked` page; Claude reports that as "Please run
//! /login". WebSocket frames are not inspected, so this route cannot be
//! dropped by it. Frame layout: `0x02 | request id (u32 BE) | kind | payload`.
use crate::codex_bridge::{self as http, ABANDON};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

pub(super) const CHANNEL: u8 = 2;
const HEADER: usize = 6;
/// Both peers cap a WebSocket message at 128 KiB.
const CHUNK: usize = 64 * 1024;
/// The server answers anything larger with 413; refuse it before upload.
const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_HEADERS: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Silence allowed between response frames, as for a direct connection.
const REPLY_TIMEOUT: Duration = Duration::from_secs(300);
const PREFIX: &str = "/api/proxy/v1/";
/// Frames waiting for the socket loop; a full queue holds request uploads back.
const QUEUE: usize = 64;

const REQUEST_HEAD: u8 = 0;
const REQUEST_BODY: u8 = 1;
const REQUEST_END: u8 = 2;
const REQUEST_CANCEL: u8 = 3;
const RESPONSE_HEAD: u8 = 0;
const RESPONSE_BODY: u8 = 1;
const RESPONSE_END: u8 = 2;

/// Send the frames the agent's requests have queued, without waiting.
pub(super) fn flush(
    bridge: Option<&Bridge>,
    socket: &mut super::transport::Socket,
) -> Result<(), String> {
    let Some(bridge) = bridge else {
        return Ok(());
    };
    for _ in 0..QUEUE {
        let Some(frame) = bridge.next_frame() else {
            break;
        };
        socket
            .send(tungstenite::Message::Binary(frame.into()))
            .map_err(|_| "remote connection lost")?;
    }
    Ok(())
}

/// Start the endpoint when `event` is a `hello` that offers it, and point the
/// agent's model requests at it.
pub(super) fn offer(
    event: &Value,
    proxy: &mut super::proxy::SessionProxy,
) -> Result<Option<Bridge>, String> {
    if !offered(event) {
        return Ok(None);
    }
    let bridge = Bridge::start(proxy.bearer())?;
    proxy.set_bridge(&bridge.origin())?;
    Ok(Some(bridge))
}

/// True when the server's `hello` offers model requests on this socket.
pub(super) fn offered(event: &Value) -> bool {
    event["type"] == "hello" && event["proxy_channel"].as_u64() == Some(u64::from(CHANNEL))
}

fn frame(id: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER + payload.len());
    frame.push(CHANNEL);
    frame.extend_from_slice(&id.to_be_bytes());
    frame.push(kind);
    frame.extend_from_slice(payload);
    frame
}

enum Reply {
    Head(u16, String),
    Body(Vec<u8>),
    End,
}

type Routes = Arc<Mutex<HashMap<u32, mpsc::Sender<Reply>>>>;

struct Shared {
    token: Zeroizing<String>,
    frames: mpsc::SyncSender<Vec<u8>>,
    routes: Routes,
    next: AtomicU32,
    shutdown: Arc<AtomicBool>,
}

impl Shared {
    fn cancel(&self, id: u32) {
        self.forget(id);
        let _ = self.frames.try_send(frame(id, REQUEST_CANCEL, &[]));
    }

    fn forget(&self, id: u32) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.remove(&id);
        }
    }
}

pub(super) struct Bridge {
    addr: SocketAddr,
    frames: mpsc::Receiver<Vec<u8>>,
    routes: Routes,
    shutdown: Arc<AtomicBool>,
}

impl Bridge {
    /// Listen on loopback. Only a caller holding the session bearer is served.
    pub(super) fn start(token: &str) -> Result<Self, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| "cannot open the local model endpoint")?;
        let addr = listener
            .local_addr()
            .map_err(|_| "cannot open the local model endpoint")?;
        let (tx, frames) = mpsc::sync_channel(QUEUE);
        let routes = Routes::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            token: Zeroizing::new(token.to_owned()),
            frames: tx,
            routes: Arc::clone(&routes),
            next: AtomicU32::new(1),
            shutdown: Arc::clone(&shutdown),
        });
        std::thread::spawn(move || accept(&listener, &shared));
        Ok(Self {
            addr,
            frames,
            routes,
            shutdown,
        })
    }

    /// The base the agent's `ANTHROPIC_BASE_URL` is built from.
    pub(super) fn origin(&self) -> String {
        format!("http://{}/", self.addr)
    }

    /// The next frame the socket loop should send, if any.
    pub(super) fn next_frame(&self) -> Option<Vec<u8>> {
        self.frames.try_recv().ok()
    }

    /// Route one server frame to the request waiting for it.
    pub(super) fn deliver(&self, frame: &[u8]) -> Result<(), String> {
        if frame.len() < HEADER || frame[0] != CHANNEL {
            return Err("invalid remote model frame".into());
        }
        let id = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]);
        let payload = &frame[HEADER..];
        let reply = match frame[5] {
            RESPONSE_HEAD => {
                let head: Value =
                    serde_json::from_slice(payload).map_err(|_| "invalid remote model frame")?;
                let status = head["status"]
                    .as_u64()
                    .and_then(|s| u16::try_from(s).ok())
                    .filter(|s| (100..600).contains(s))
                    .ok_or("invalid remote model frame")?;
                let content_type = head["content_type"]
                    .as_str()
                    .filter(|t| !t.contains(['\r', '\n']))
                    .unwrap_or("application/json");
                Reply::Head(status, content_type.to_owned())
            }
            RESPONSE_BODY => Reply::Body(payload.to_vec()),
            RESPONSE_END => Reply::End,
            _ => return Err("invalid remote model frame".into()),
        };
        let mut routes = self
            .routes
            .lock()
            .map_err(|_| "remote model bridge failed")?;
        let end = matches!(reply, Reply::End);
        // A request the agent abandoned has no route; its late frames are dropped.
        if let Some(route) = routes.get(&id) {
            if route.send(reply).is_err() || end {
                routes.remove(&id);
            }
        }
        Ok(())
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Dropping the routes answers every waiting request with 502.
        if let Ok(mut routes) = self.routes.lock() {
            routes.clear();
        }
        // Wake the accept loop so it sees the shutdown flag.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
    }
}

fn accept(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        if let Ok(stream) = stream {
            let shared = Arc::clone(shared);
            std::thread::spawn(move || serve(stream, &shared));
        }
    }
}

fn authorized(request: &http::ParsedRequest, token: &str) -> bool {
    let bearer = request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "));
    [bearer, request.header("x-api-key")]
        .into_iter()
        .flatten()
        .any(|candidate| http::constant_time_eq(candidate.as_bytes(), token.as_bytes()))
}

/// The proxied endpoint, or `None` for anything outside `/api/proxy/v1/`.
fn endpoint(path: &str) -> Option<&str> {
    let endpoint = path.strip_prefix(PREFIX)?;
    Some(endpoint.split_once('?').map_or(endpoint, |(path, _)| path)).filter(|e| !e.is_empty())
}

/// Read and check one request. `Err` carries the status already chosen for it.
fn admit(stream: &mut TcpStream, shared: &Shared) -> Result<(String, Value, Vec<u8>), u16> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut request = http::read_headers(stream, MAX_HEADERS, deadline, &shared.shutdown)?;
    if !authorized(&request, &shared.token) {
        return Err(401);
    }
    let endpoint = endpoint(&request.path).ok_or(404_u16)?.to_owned();
    if request.content_length > MAX_BODY {
        return Err(413);
    }
    let prefix = std::mem::take(&mut request.body_prefix);
    let body = http::read_body(
        stream,
        prefix,
        request.content_length,
        deadline,
        &shared.shutdown,
    )?;
    let head = json!({
        "method": request.method,
        "endpoint": endpoint,
        "anthropic_version": request.header("anthropic-version"),
        "anthropic_beta": request.header("anthropic-beta"),
    });
    Ok((endpoint, head, body))
}

fn serve(mut stream: TcpStream, shared: &Shared) {
    let (_, head, body) = match admit(&mut stream, shared) {
        Ok(admitted) => admitted,
        Err(ABANDON) => return,
        Err(status) => {
            let _ = http::write_error(&mut stream, status);
            return;
        }
    };
    let id = shared.next.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    match shared.routes.lock() {
        Ok(mut routes) => routes.insert(id, tx),
        Err(_) => return,
    };
    if upload(shared, id, &head, &body).is_err() {
        shared.forget(id);
        let _ = http::write_error(&mut stream, 502);
        return;
    }
    relay(&mut stream, &rx, shared, id);
    shared.forget(id);
}

fn upload(shared: &Shared, id: u32, head: &Value, body: &[u8]) -> Result<(), String> {
    let send = |kind, payload: &[u8]| {
        shared
            .frames
            .send(frame(id, kind, payload))
            .map_err(|_| "remote session closed".to_owned())
    };
    send(REQUEST_HEAD, head.to_string().as_bytes())?;
    for chunk in body.chunks(CHUNK) {
        send(REQUEST_BODY, chunk)?;
    }
    send(REQUEST_END, &[])
}

/// Write the server's answer as it arrives: status and type, then a chunked
/// body flushed per frame so streamed tokens are not held back.
fn relay(stream: &mut TcpStream, replies: &mpsc::Receiver<Reply>, shared: &Shared, id: u32) {
    let mut started = false;
    loop {
        let written = match replies.recv_timeout(REPLY_TIMEOUT) {
            Ok(Reply::Head(status, content_type)) if !started => {
                started = true;
                write_head(stream, status, &content_type)
            }
            Ok(Reply::Body(chunk)) if started => write_chunk(stream, &chunk),
            Ok(Reply::End) if started => {
                let _ = stream.write_all(b"0\r\n\r\n").and_then(|()| stream.flush());
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
            // An out-of-order frame, a timeout or a lost session.
            other => {
                if !matches!(other, Err(mpsc::RecvTimeoutError::Disconnected)) {
                    shared.cancel(id);
                }
                if !started {
                    let _ = http::write_error(stream, 502);
                }
                return;
            }
        };
        if written.is_err() {
            // The agent hung up; stop the upstream call it no longer wants.
            shared.cancel(id);
            return;
        }
    }
}

fn write_head(stream: &mut TcpStream, status: u16, content_type: &str) -> io::Result<()> {
    let reason = if (200..300).contains(&status) {
        "OK"
    } else {
        "Error"
    };
    stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()
}

/// One chunked-encoding chunk. An empty one would end the body, so it is skipped.
fn write_chunk(stream: &mut TcpStream, chunk: &[u8]) -> io::Result<()> {
    if chunk.is_empty() {
        return Ok(());
    }
    write!(stream, "{:x}\r\n", chunk.len())?;
    stream.write_all(chunk)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod tests;

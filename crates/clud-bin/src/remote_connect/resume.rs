//! Reconnecting a remote session after an unexpected loss of its socket.
//!
//! A server restart, a dropped network or a silent (half-open) connection must
//! not end a session the user is in the middle of: the local shell keeps
//! running, its output is held in a bounded buffer, and the client reconnects
//! with the session token it already holds. The server's `hello.resume` then
//! says what consent it still records; the client reconciles against its own
//! (see `State::resume` in `mod.rs`). The protocol is clud.bot's SPEC §4.2.3;
//! the design is `docs/architecture/remote-assistance.md#session-resume`.
use super::transport::{self, ConnectError, Socket};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::Message;

/// How long the client keeps trying before it gives up. The server keeps the
/// session for its own grace window (300 s by default), measured from when it
/// noticed the loss or restarted.
pub(super) const RESUME_WINDOW: Duration = Duration::from_secs(300);
/// Shortens [`RESUME_WINDOW`] (never lengthens it), for tests.
const WINDOW_ENV: &str = "CLUD_REMOTE_RESUME_WINDOW_SECS";
pub(super) const BACKOFF_BASE: Duration = Duration::from_millis(500);
pub(super) const BACKOFF_CAP: Duration = Duration::from_secs(15);
/// After a `server_restart` notice: the new instance needs a moment to start.
const RESTART_DELAY: Duration = Duration::from_secs(2);
/// A ping goes out this often; the server answers each with a pong.
pub(super) const HEARTBEAT: Duration = Duration::from_secs(10);
/// Nothing heard for this long means the connection is dead even if TCP has
/// not noticed (a half-open connection: no FIN, packets dropped).
pub(super) const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(35);
/// Shell output held while disconnected. Past it the oldest output is dropped
/// and the operator is shown how much is missing.
pub(super) const OUTAGE_BYTES: usize = 1024 * 1024;

pub(super) const LOST_NOTICE: &str = "Connection to clud.bot lost; reconnecting… (Ctrl+C to end)";
pub(super) const RESUMED_NOTICE: &str = "Reconnected.";
pub(super) const SUPERSEDED: &str = "This session was resumed from another connection.";
pub(super) const GAVE_UP: &str =
    "could not reconnect to clud.bot in time; the session has ended. Run `clud remote connect clud.bot` again.";

pub(super) fn window() -> Duration {
    window_from(std::env::var(WINDOW_ENV).ok().as_deref())
}

fn window_from(value: Option<&str>) -> Duration {
    value
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .map_or(RESUME_WINDOW, |shorter| {
            shorter.clamp(Duration::from_secs(1), RESUME_WINDOW)
        })
}

/// Capped exponential backoff with jitter: attempt `n` waits a random time in
/// `[step/2, step]`, where `step = min(BACKOFF_CAP, BACKOFF_BASE * 2^n)`.
#[derive(Default)]
pub(super) struct Backoff {
    attempt: u32,
}

impl Backoff {
    pub(super) fn next(&mut self) -> Duration {
        self.next_with(jitter_fraction())
    }

    fn next_with(&mut self, jitter: f64) -> Duration {
        let step = BACKOFF_BASE
            .saturating_mul(1 << self.attempt.min(16))
            .min(BACKOFF_CAP);
        self.attempt = self.attempt.saturating_add(1);
        step - step.mul_f64(0.5 * jitter.clamp(0.0, 1.0))
    }

    /// The first wait after a `server_restart` notice, jittered so a restart
    /// does not bring every client back in the same instant.
    pub(super) fn after_restart(&mut self) -> Duration {
        RESTART_DELAY + BACKOFF_BASE.mul_f64(2.0 * jitter_fraction())
    }
}

fn jitter_fraction() -> f64 {
    let mut bytes = [0_u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return 0.5;
    }
    f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX)
}

/// What a close frame from the server means.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Close {
    /// The connection went away; the session may still be live. Reconnecting
    /// asks the server, which refuses an ended session.
    Lost,
    /// The session cannot continue on any connection from this client.
    Final(&'static str),
}

pub(super) fn close_meaning(frame: Option<&CloseFrame>) -> Close {
    match frame.map(|frame| u16::from(frame.code)) {
        Some(4409) => Close::Final(SUPERSEDED),
        // Protocol violations: reconnecting would repeat them.
        Some(1008 | 1009) => Close::Final("remote connection closed"),
        // An encrypted record failed authentication, on either side.
        Some(super::e2e::FAILED_CLOSE) => Close::Final(super::e2e::FAILED),
        _ => Close::Lost,
    }
}

/// Shell output produced while disconnected, oldest dropped first.
#[derive(Default)]
pub(super) struct Outage {
    frames: VecDeque<Message>,
    bytes: usize,
    dropped: usize,
    /// The terminal the output belongs to, captured when the link was lost:
    /// the shell may exit while offline, and its exit still has to be reported.
    pty: Option<Value>,
}

impl Outage {
    pub(super) fn begin(&mut self, pty: Option<Value>) {
        if self.pty.is_none() {
            self.pty = pty;
        }
    }

    pub(super) fn pty(&self) -> Option<Value> {
        self.pty.clone()
    }

    /// Hold one frame. A text frame (`pty_exit`) is never dropped.
    pub(super) fn push(&mut self, message: Message) {
        if let Message::Binary(frame) = &message {
            self.bytes += frame.len().saturating_sub(1);
        }
        self.frames.push_back(message);
        while self.bytes > OUTAGE_BYTES {
            let Some(index) = self
                .frames
                .iter()
                .position(|frame| matches!(frame, Message::Binary(_)))
            else {
                break;
            };
            if let Some(Message::Binary(frame)) = self.frames.remove(index) {
                let size = frame.len().saturating_sub(1);
                self.bytes -= size;
                self.dropped += size;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn held_bytes(&self) -> usize {
        self.bytes
    }

    /// The gap notice, if output was dropped, then the held frames in order.
    /// Leaves the buffer empty for the next outage.
    pub(super) fn take(&mut self) -> (Option<Value>, VecDeque<Message>) {
        let pty_id = self
            .pty
            .take()
            .and_then(|pty| pty["pty_id"].as_str().map(str::to_owned));
        let dropped = std::mem::take(&mut self.dropped);
        self.bytes = 0;
        let gap =
            (dropped > 0).then(|| json!({"type":"pty_gap", "pty_id":pty_id, "bytes":dropped}));
        (gap, std::mem::take(&mut self.frames))
    }

    /// Forget everything: control ended, so none of it may be sent.
    pub(super) fn discard(&mut self) {
        let _ = self.take();
    }

    pub(super) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The oldest held frame, to send now.
    pub(super) fn pop_front(&mut self) -> Option<Message> {
        let frame = self.frames.pop_front()?;
        if let Message::Binary(bytes) = &frame {
            self.bytes -= bytes.len().saturating_sub(1);
        }
        Some(frame)
    }

    /// Put frames that could not be sent back in front, for the next attempt.
    pub(super) fn requeue(&mut self, frames: VecDeque<Message>) {
        for frame in frames.into_iter().rev() {
            if let Message::Binary(bytes) = &frame {
                self.bytes += bytes.len().saturating_sub(1);
            }
            self.frames.push_front(frame);
        }
    }
}

/// What one reconnection attempt found.
pub(super) enum Attempt {
    Connected(Box<Socket>),
    /// The server refused the token and reports the session over.
    Ended(&'static str),
    Failed,
    /// Not end-to-end encrypted as required, or the key exchange failed.
    Insecure(String),
}

/// The status the server reports for a refused token, as the end message.
fn ended(base: &url::Url, session: &transport::Session) -> Option<&'static str> {
    transport::terminal_status(base, session)
        .as_deref()
        .and_then(super::end_message)
}

/// Reconnects in the background, so the caller keeps draining the shell and
/// sees Ctrl+C while a handshake waits on an unreachable server.
pub(super) struct Reconnector {
    base: url::Url,
    session: transport::Session,
    backoff: Backoff,
    deadline: Instant,
    next: Instant,
    pending: Option<mpsc::Receiver<Attempt>>,
}

/// What [`Reconnector::poll`] found.
pub(super) enum Poll {
    Waiting,
    Connected(Box<Socket>),
    Ended(&'static str),
    GaveUp,
    /// Final: the new socket would not be safely encrypted.
    Insecure(String),
}

impl Reconnector {
    pub(super) fn new(base: &url::Url, session: &transport::Session, restart: bool) -> Self {
        let mut backoff = Backoff::default();
        let first = if restart {
            backoff.after_restart()
        } else {
            backoff.next()
        };
        let now = Instant::now();
        Self {
            base: base.clone(),
            session: session.clone(),
            backoff,
            deadline: now + window(),
            next: now + first,
            pending: None,
        }
    }

    pub(super) fn poll(&mut self) -> Poll {
        if let Some(pending) = &self.pending {
            match pending.try_recv() {
                Ok(Attempt::Connected(socket)) => return Poll::Connected(socket),
                Ok(Attempt::Ended(message)) => return Poll::Ended(message),
                Ok(Attempt::Insecure(message)) => return Poll::Insecure(message),
                Ok(Attempt::Failed) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.next = Instant::now() + self.backoff.next();
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let now = Instant::now();
        if now >= self.deadline {
            return Poll::GaveUp;
        }
        if self.pending.is_none() && now >= self.next {
            self.pending = Some(self.start());
        }
        Poll::Waiting
    }

    fn start(&self) -> mpsc::Receiver<Attempt> {
        let (tx, rx) = mpsc::sync_channel(1);
        let session = self.session.clone();
        let base = self.base.clone();
        // Detached: an abandoned attempt ends at its own handshake timeout.
        std::thread::spawn(move || {
            // A fresh exchange: every socket has its own ephemeral keys.
            let outcome = match transport::connect_socket(&session) {
                Ok(socket) => Attempt::Connected(Box::new(socket)),
                Err(ConnectError::Refused) => {
                    ended(&base, &session).map_or(Attempt::Failed, Attempt::Ended)
                }
                Err(ConnectError::Unreachable(_)) => Attempt::Failed,
                Err(ConnectError::Insecure(message)) => Attempt::Insecure(message),
            };
            let _ = tx.send(outcome);
        });
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tungstenite::protocol::frame::coding::CloseCode;

    #[test]
    fn backoff_doubles_to_its_cap_with_jitter_in_the_upper_half() {
        let mut backoff = Backoff::default();
        assert_eq!(backoff.next_with(0.0), BACKOFF_BASE);
        assert_eq!(backoff.next_with(0.0), BACKOFF_BASE * 2);
        assert_eq!(backoff.next_with(1.0), BACKOFF_BASE * 2);
        for _ in 0..40 {
            let delay = backoff.next_with(0.0);
            assert!(delay <= BACKOFF_CAP);
        }
        assert_eq!(backoff.next_with(0.0), BACKOFF_CAP);
        assert_eq!(backoff.next_with(1.0), BACKOFF_CAP / 2);
        let delay = Backoff::default().next();
        assert!(delay >= BACKOFF_BASE / 2 && delay <= BACKOFF_BASE);
    }

    #[test]
    fn the_resume_window_is_five_minutes_and_can_only_be_shortened() {
        assert_eq!(RESUME_WINDOW, Duration::from_secs(300));
        assert_eq!(window_from(None), RESUME_WINDOW);
        assert_eq!(window_from(Some("20")), Duration::from_secs(20));
        assert_eq!(window_from(Some("0")), Duration::from_secs(1));
        assert_eq!(window_from(Some("86400")), RESUME_WINDOW);
        assert_eq!(window_from(Some("soon")), RESUME_WINDOW);
    }

    #[test]
    fn a_heartbeat_timeout_spans_several_pings() {
        assert!(HEARTBEAT_TIMEOUT >= HEARTBEAT * 3);
        // The server closes a socket silent for 60 s; the client pings well inside it.
        assert!(HEARTBEAT < Duration::from_secs(20));
    }

    fn close(code: u16) -> CloseFrame {
        CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        }
    }

    #[test]
    fn only_a_superseded_or_violating_session_is_final() {
        assert_eq!(close_meaning(Some(&close(4409))), Close::Final(SUPERSEDED));
        assert!(matches!(close_meaning(Some(&close(1008))), Close::Final(_)));
        for code in [1000, 1001, 1006, 1011, 1012, 1013, 4403] {
            assert_eq!(close_meaning(Some(&close(code))), Close::Lost, "{code}");
        }
        assert_eq!(close_meaning(None), Close::Lost);
    }

    fn output(size: usize) -> Message {
        let mut frame = vec![0];
        frame.resize(size + 1, b'x');
        Message::Binary(frame.into())
    }

    #[test]
    fn an_outage_flood_keeps_the_newest_megabyte_and_counts_the_rest() {
        let mut outage = Outage::default();
        outage.begin(Some(json!({"pty_id":"shell", "cols":80, "rows":24})));
        for _ in 0..200 {
            outage.push(output(16 * 1024));
        }
        outage.push(Message::Text(r#"{"type":"pty_exit"}"#.into()));
        assert!(outage.held_bytes() <= OUTAGE_BYTES);
        let (gap, frames) = outage.take();
        let gap = gap.expect("dropped output is reported");
        assert_eq!(gap["pty_id"], "shell");
        assert_eq!(
            gap["bytes"].as_u64().unwrap() as usize,
            200 * 16 * 1024 - OUTAGE_BYTES
        );
        assert!(
            matches!(frames.back(), Some(Message::Text(_))),
            "exit kept last"
        );
        assert_eq!(frames.len(), OUTAGE_BYTES / (16 * 1024) + 1);
        assert_eq!(outage.held_bytes(), 0);
        assert!(outage.take().0.is_none(), "one gap per outage");
    }

    #[test]
    fn an_outage_that_fits_has_no_gap_and_keeps_order() {
        let mut outage = Outage::default();
        outage.push(output(3));
        outage.push(output(5));
        let (gap, mut frames) = outage.take();
        assert!(gap.is_none());
        let first = frames.pop_front().unwrap();
        outage.requeue(frames);
        assert_eq!(outage.held_bytes(), 5);
        assert!(matches!(first, Message::Binary(b) if b.len() == 4));
    }
}

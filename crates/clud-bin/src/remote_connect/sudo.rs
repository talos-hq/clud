//! "Request sudo": the client's half of the operator's sudo request.
//!
//! The operator asks (`sudo_request`); this client asks the user, reads the
//! password locally with echo off, and hands it over a private socket to the
//! PTY's session leader (`leader.rs`, `platform::remote_shell`), which
//! validates sudo inside the operator shell's session. The server only ever
//! hears a closed set of states (`sudo_state`), never text and never the
//! password. See `docs/architecture/remote-assistance.md#request-sudo`.

use super::console::Console;
use super::leader::{self, Reply};
use crate::platform::console_echo::{self, EchoGuard};
use crate::platform::remote_shell::LeaderStream;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::sync::mpsc;
use std::time::Duration;
use zeroize::Zeroizing;

const OPERATOR: &str = "Zach Vorhies";
/// How long revoke, disconnect and Ctrl+C wait for the leader's `sudo -k`.
const DROP_WAIT: Duration = Duration::from_secs(1);

/// The states the server may hear. A closed set: no free text ever leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Wire {
    Prompting,
    /// `Some(until)` is the Unix time the 4-hour cap ends it; `None` when
    /// this account needs no password, so there is nothing to keep alive.
    Active(Option<u64>),
    Denied,
    Unavailable,
    Dropped,
    Expired,
}

impl Wire {
    fn name(self) -> &'static str {
        match self {
            Self::Prompting => "prompting",
            Self::Active(_) => "active",
            Self::Denied => "denied",
            Self::Unavailable => "unavailable",
            Self::Dropped => "dropped",
            Self::Expired => "expired",
        }
    }
}

/// `{"type":"sudo_state", "request_id"?, "state", "until"?}` and nothing else.
pub(super) fn state_event(request_id: Option<&str>, state: Wire) -> Value {
    let mut event = json!({"type":"sudo_state", "state":state.name()});
    if let Some(id) = request_id {
        event["request_id"] = json!(id);
    }
    if let Wire::Active(Some(until)) = state {
        event["until"] = json!(until);
    }
    event
}

/// Whether to advertise `sudo`: a host whose sudo tickets a session leader can
/// share (not Windows), and a terminal on which the password can be hidden.
pub(super) fn advertised(inline: bool) -> bool {
    crate::platform::remote_shell::SUPPORTED && inline
}

/// The question, exactly as the user sees it.
pub(super) fn consent_lines() -> [String; 3] {
    [
        format!("{OPERATOR} is asking for administrator (sudo) access for up to 4 hours."),
        "Changes made as root stay after sudo ends.".to_owned(),
        crate::yes_no::question(
            &format!("Allow sudo for {OPERATOR}?"),
            crate::yes_no::Default::No,
        ),
    ]
}

fn password_prompt(user: &str) -> String {
    format!("Password for {user} (typing is hidden; it is never sent to clud.bot): ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Asked the leader whether a password is needed at all.
    Probing,
    Confirm,
    /// Waiting for password attempt `n` (1-based).
    Password(u8),
    Validating(u8),
    Active(Option<u64>),
    /// Sent `drop`; ignore every reply until the leader confirms it.
    Dropping,
}

/// The authenticated socket to the leader: a writer and a reply stream.
pub(super) struct Link {
    writer: Box<dyn Write + Send>,
    replies: mpsc::Receiver<Reply>,
}

impl Link {
    pub fn new(stream: LeaderStream) -> io::Result<Self> {
        let mut reader = stream.try_clone()?;
        let (tx, replies) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = Zeroizing::new(Vec::new());
            while let Ok(true) = leader::read_line(&mut reader, &mut line) {
                let parsed = std::str::from_utf8(&line).ok().and_then(Reply::parse);
                if parsed.is_some_and(|reply| tx.send(reply).is_err()) {
                    break;
                }
            }
        });
        Ok(Self {
            writer: Box::new(stream),
            replies,
        })
    }

    #[cfg(test)]
    pub fn fake(writer: Box<dyn Write + Send>, replies: mpsc::Receiver<Reply>) -> Self {
        Self { writer, replies }
    }

    fn send(&mut self, line: &[u8]) -> io::Result<()> {
        self.writer.write_all(line)?;
        self.writer.flush()
    }
}

pub(super) struct Sudo {
    link: Option<Link>,
    phase: Phase,
    request: Option<String>,
    /// Held only while the password is being typed.
    echo: Option<EchoGuard>,
    user: String,
}

impl Sudo {
    pub fn new() -> Self {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "this account".to_owned());
        Self {
            link: None,
            phase: Phase::Idle,
            request: None,
            echo: None,
            user,
        }
    }

    /// A new PTY's leader connected.
    pub fn attach(&mut self, link: Link) {
        self.link = Some(link);
        self.phase = Phase::Idle;
        self.request = None;
    }

    /// Whether the next typed line belongs to this flow.
    pub fn wants_line(&self) -> bool {
        matches!(self.phase, Phase::Confirm | Phase::Password(_))
    }

    /// The operator clicked Request sudo. `ready` is control granted with a
    /// shell running.
    pub fn on_request(
        &mut self,
        request_id: Option<&str>,
        ready: bool,
        console: &Console,
    ) -> Option<Value> {
        match self.phase {
            Phase::Idle => {}
            Phase::Active(until) => return Some(state_event(request_id, Wire::Active(until))),
            // One question at a time; a repeated click changes nothing.
            _ => return None,
        }
        let Some(link) = self.link.as_mut().filter(|_| ready) else {
            return Some(state_event(request_id, Wire::Unavailable));
        };
        if link.send(b"probe\n").is_err() {
            return Some(state_event(request_id, Wire::Unavailable));
        }
        self.request = request_id.map(str::to_owned);
        self.phase = Phase::Probing;
        console.hold(true);
        Some(self.event(Wire::Prompting))
    }

    /// The operator clicked Drop sudo, or withdrew a pending request.
    pub fn on_drop(&mut self, console: &Console) -> Option<Value> {
        self.echo = None;
        let sent = self
            .link
            .as_mut()
            .is_some_and(|link| link.send(b"drop\n").is_ok());
        if sent {
            if self.wants_line() {
                console.answered(false);
            }
            self.phase = Phase::Dropping;
            return None;
        }
        Some(self.finish(
            Wire::Dropped,
            &format!("Sudo for {OPERATOR} is off."),
            console,
        ))
    }

    /// A line the user typed while this flow wanted one.
    pub fn on_line(&mut self, line: String, console: &Console) -> Option<Value> {
        match self.phase {
            Phase::Confirm => {
                console.answered(true);
                if crate::yes_no::answer(&line, crate::yes_no::Default::No) {
                    self.ask_password(1, console)
                } else {
                    Some(self.finish(Wire::Denied, "Sudo declined.", console))
                }
            }
            Phase::Password(attempt) => {
                self.submit(Zeroizing::new(line.into_bytes()), attempt, console)
            }
            _ => None,
        }
    }

    fn ask_password(&mut self, attempt: u8, console: &Console) -> Option<Value> {
        // No way to hide the typing means no password is asked for.
        let Ok(guard) = console_echo::suppress_stdin_echo() else {
            return Some(self.finish(
                Wire::Unavailable,
                "[clud] Cannot hide typing on this terminal; sudo was not enabled.",
                console,
            ));
        };
        self.echo = Some(guard);
        console.prompt(password_prompt(&self.user));
        self.phase = Phase::Password(attempt);
        None
    }

    fn submit(
        &mut self,
        mut secret: Zeroizing<Vec<u8>>,
        attempt: u8,
        console: &Console,
    ) -> Option<Value> {
        self.echo = None;
        // ECHONL already moved the cursor past the hidden line.
        console.answered(true);
        while matches!(secret.last(), Some(b'\n' | b'\r')) {
            secret.pop();
        }
        if secret.is_empty() {
            return Some(self.finish(Wire::Denied, "Sudo declined.", console));
        }
        let sent = self
            .link
            .as_mut()
            .is_some_and(|link| leader::write_validate(&mut link.writer, &secret).is_ok());
        drop(secret);
        if !sent {
            return Some(self.finish(Wire::Unavailable, "[clud] The shell has ended.", console));
        }
        self.phase = Phase::Validating(attempt);
        None
    }

    /// Handle what the leader said, if anything. Call once per session loop.
    pub fn poll(&mut self, console: &Console) -> Option<Value> {
        let next = self.link.as_ref()?.replies.try_recv();
        match next {
            Ok(reply) => self.on_reply(reply, console),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                // The shell (and with it the leader and its ticket) ended.
                self.link = None;
                (self.phase != Phase::Idle).then(|| {
                    self.finish(
                        Wire::Dropped,
                        &format!("Sudo for {OPERATOR} is off."),
                        console,
                    )
                })
            }
        }
    }

    fn on_reply(&mut self, reply: Reply, console: &Console) -> Option<Value> {
        let phase = self.phase;
        match (reply, phase) {
            (Reply::Dropped, _) => Some(self.finish(
                Wire::Dropped,
                &format!("Sudo for {OPERATOR} is off."),
                console,
            )),
            (_, Phase::Dropping) => None,
            (Reply::NeedsPassword, Phase::Probing) => {
                let [first, second, question] = consent_lines();
                console.line(first);
                console.line(second);
                console.prompt(question);
                self.phase = Phase::Confirm;
                None
            }
            (Reply::NoPassword, Phase::Probing) => {
                console.hold(false);
                console.line(format!(
                    "[clud] {OPERATOR} can already use sudo on this computer without a password."
                ));
                self.phase = Phase::Active(None);
                Some(self.event(Wire::Active(None)))
            }
            (Reply::Wrong, Phase::Validating(attempt)) if attempt < leader::MAX_ATTEMPTS => {
                console.line(format!(
                    "Sorry, that password was not accepted ({attempt} of {}).",
                    leader::MAX_ATTEMPTS
                ));
                self.ask_password(attempt + 1, console)
            }
            (Reply::Wrong, Phase::Validating(_)) => Some(self.finish(
                Wire::Denied,
                "Sudo was not enabled: the password was not accepted.",
                console,
            )),
            (Reply::Active { until }, Phase::Validating(_)) => {
                console.hold(false);
                console.line(format!(
                    "Sudo is on for {OPERATOR} for up to 4 hours. \
                     Press Ctrl+C to end the session and sudo."
                ));
                self.phase = Phase::Active(Some(until));
                Some(self.event(Wire::Active(Some(until))))
            }
            (Reply::Unavailable, _) => Some(self.finish(
                Wire::Unavailable,
                "[clud] sudo is not available here: this account has no sudo rights, \
                 there is no setuid sudo, or its sudo policy cannot be shared.",
                console,
            )),
            (Reply::Expired, _) => Some(self.finish(
                Wire::Expired,
                &format!("Sudo for {OPERATOR} ended after 4 hours; it must be approved again."),
                console,
            )),
            _ => None,
        }
    }

    fn event(&self, state: Wire) -> Value {
        state_event(self.request.as_deref(), state)
    }

    fn finish(&mut self, state: Wire, message: &str, console: &Console) -> Value {
        self.echo = None;
        console.hold(false);
        console.line(message);
        self.phase = Phase::Idle;
        let event = self.event(state);
        self.request = None;
        event
    }

    /// Revoke, disconnect, Ctrl+C, a killed or restarted shell: drop sudo
    /// before the PTY is killed, waiting briefly for the leader's `sudo -k`.
    /// The leader's own exit invalidates the ticket too, so this is the
    /// first of two locks, not the only one.
    pub fn shutdown(&mut self, console: &Console) {
        let asked = self.wants_line() || self.phase == Phase::Probing;
        self.release();
        console.hold(false);
        if asked {
            console.answered(false);
            console.line("Sudo request cancelled.");
        }
    }

    fn release(&mut self) {
        self.echo = None;
        self.phase = Phase::Idle;
        self.request = None;
        let Some(mut link) = self.link.take() else {
            return;
        };
        if link.send(b"drop\n").is_ok() {
            let deadline = std::time::Instant::now() + DROP_WAIT;
            while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
                match link.replies.recv_timeout(left) {
                    Ok(Reply::Dropped) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    }
}

impl Drop for Sudo {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
#[path = "sudo_tests.rs"]
mod tests;

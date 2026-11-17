//! The portable half of `clud-remote-shell`, the PTY's session leader.
//!
//! A sudo ticket under the default `timestamp_type=tty` is keyed by uid, tty,
//! session ID and the session leader's start time, so only a process inside
//! the operator PTY's session can create one the operator's shell will use.
//! The leader is that process. It validates with the password the client
//! hands it, keeps the ticket alive, and drops it again. This module holds
//! what does not name a host: the client <-> leader line protocol, how sudo's
//! output is classified, the keep-alive and 4-hour cap clock, and running
//! sudo itself through running-process. The socket, signals, `prctl` and the
//! shell spawn live in `platform::remote_shell` (Unix only).
//!
//! See `docs/architecture/remote-assistance.md#request-sudo` and DD-216.

use running_process::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;

/// The client's listening socket, read once by the leader.
pub const SOCKET_ENV: &str = "CLUD_REMOTE_LEADER_SOCKET";
/// Proof the connecting leader is the one this client started.
pub const NONCE_ENV: &str = "CLUD_REMOTE_LEADER_NONCE";
/// The client's answer once it has verified the connecting leader; the
/// leader starts no shell before it.
pub const ACK: &[u8] = b"ok";
/// The multicall name the PTY's first process runs under.
pub const PROGRAM: &str = "clud-remote-shell";
/// Below every packaged `timestamp_timeout` (5 or 15 minutes).
pub const KEEPALIVE: Duration = Duration::from_secs(60);
/// Extended sudo ends this long after approval; the user must approve again.
pub const CAP: Duration = Duration::from_secs(4 * 60 * 60);
/// Password attempts per request, matching sudo's `passwd_tries`.
pub const MAX_ATTEMPTS: u8 = 3;
/// The only environment sudo sees: no `SUDO_ASKPASS`, no `LD_*`.
const SUDO_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
/// A wrong password costs sudo about two seconds; PAM can be slower.
const SUDO_DEADLINE: Duration = Duration::from_secs(30);
/// Longest request line, so a broken peer cannot grow a buffer forever.
const MAX_LINE: usize = 4096;

/// Leader -> client. Never carries sudo's own text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    NeedsPassword,
    /// sudo works here without a password; nothing was done.
    NoPassword,
    /// Ticket validated and shared with the session, until this Unix time.
    Active {
        until: u64,
    },
    Wrong,
    /// No sudo rights, no setuid sudo, or a policy the session cannot share.
    Unavailable,
    Dropped,
    Expired,
}

impl Reply {
    pub fn line(&self) -> String {
        match self {
            Self::NeedsPassword => "needs-password\n".into(),
            Self::NoPassword => "no-password\n".into(),
            Self::Active { until } => format!("active {until}\n"),
            Self::Wrong => "wrong\n".into(),
            Self::Unavailable => "unavailable\n".into(),
            Self::Dropped => "dropped\n".into(),
            Self::Expired => "expired\n".into(),
        }
    }

    pub fn parse(line: &str) -> Option<Self> {
        Some(match line.trim_end() {
            "needs-password" => Self::NeedsPassword,
            "no-password" => Self::NoPassword,
            "wrong" => Self::Wrong,
            "unavailable" => Self::Unavailable,
            "dropped" => Self::Dropped,
            "expired" => Self::Expired,
            other => Self::Active {
                until: other.strip_prefix("active ")?.parse().ok()?,
            },
        })
    }
}

/// Client -> leader. Deliberately not `Debug`: `Validate` holds the password.
pub enum Request {
    Probe,
    Validate(Zeroizing<Vec<u8>>),
    Drop,
}

/// Read one line without a userspace buffer that would keep a copy of the
/// password after it is zeroed. `Ok(false)` is end of stream.
pub fn read_line(reader: &mut impl Read, line: &mut Zeroizing<Vec<u8>>) -> io::Result<bool> {
    line.clear();
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte)? == 0 {
            return Ok(!line.is_empty());
        }
        if byte[0] == b'\n' {
            return Ok(true);
        }
        if line.len() >= MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        line.push(byte[0]);
    }
}

/// The next request, `None` at end of stream. Unknown lines are skipped.
pub fn read_request(reader: &mut impl Read) -> io::Result<Option<Request>> {
    let mut line = Zeroizing::new(Vec::new());
    loop {
        if !read_line(reader, &mut line)? {
            return Ok(None);
        }
        match line.as_slice() {
            b"probe" => return Ok(Some(Request::Probe)),
            b"drop" => return Ok(Some(Request::Drop)),
            b"validate" => {
                let mut secret = Zeroizing::new(Vec::new());
                if !read_line(reader, &mut secret)? {
                    return Ok(None);
                }
                return Ok(Some(Request::Validate(secret)));
            }
            _ => {}
        }
    }
}

/// Send `secret` for validation. The caller owns and zeroes `secret`.
pub fn write_validate(writer: &mut impl Write, secret: &[u8]) -> io::Result<()> {
    let mut frame = Zeroizing::new(Vec::with_capacity(secret.len() + 10));
    frame.extend_from_slice(b"validate\n");
    frame.extend_from_slice(secret);
    frame.push(b'\n');
    writer.write_all(&frame)?;
    writer.flush()
}

/// What sudo's exit code and `LC_ALL=C` output mean. Classic sudo 1.9 and
/// sudo-rs 0.2 wordings, from the container experiments behind DD-216.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    NoRights,
    NeedsPassword,
    Other,
}

pub fn classify(code: i32, output: &str) -> Outcome {
    if code == 0 {
        return Outcome::Ok;
    }
    let text = output.to_ascii_lowercase();
    let no_rights = [
        "may not run sudo",
        "is not in the sudoers",
        "is not allowed to",
        "afraid i can't do that",
    ];
    if no_rights.iter().any(|needle| text.contains(needle)) {
        return Outcome::NoRights;
    }
    if text.contains("a password is required") || text.contains("authentication is required") {
        return Outcome::NeedsPassword;
    }
    Outcome::Other
}

/// What the keep-alive clock asks for next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    Wait,
    Refresh,
    Expire,
}

/// A validated ticket's clock: refreshed every [`KEEPALIVE`], ended at [`CAP`].
#[derive(Debug, Clone, Copy)]
pub struct Ticket {
    approved: Instant,
    refreshed: Instant,
}

impl Ticket {
    pub fn new(now: Instant) -> Self {
        Self {
            approved: now,
            refreshed: now,
        }
    }

    pub fn due(&self, now: Instant, keepalive: Duration, cap: Duration) -> Due {
        if now.saturating_duration_since(self.approved) >= cap {
            Due::Expire
        } else if now.saturating_duration_since(self.refreshed) >= keepalive {
            Due::Refresh
        } else {
            Due::Wait
        }
    }

    pub fn refreshed(&mut self, now: Instant) {
        self.refreshed = now;
    }
}

/// A verified sudo executable, run with pipes for every stream so nothing it
/// prints reaches the PTY, and with no new session or process group so the
/// ticket it writes is keyed to the PTY's session.
pub struct Sudo {
    path: PathBuf,
}

impl Sudo {
    /// `path` must already be verified (`platform::remote_shell` checks
    /// root ownership and the setuid bit); tests pass a stub.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> (i32, String) {
        let mut argv = vec![self.path.to_string_lossy().into_owned()];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        run_piped(argv, stdin)
    }

    /// `sudo -n -v` from a grandchild with a different parent: true when the
    /// ticket is keyed by tty and session, so the operator's shell shares it.
    /// False under `timestamp_type=ppid` or `timestamp_timeout=0`.
    fn shared(&self) -> bool {
        let script = "\"$0\" -n -v; status=$?; exit \"$status\"";
        let argv = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            script.to_owned(),
            self.path.to_string_lossy().into_owned(),
        ];
        run_piped(argv, None).0 == 0
    }
}

fn run_piped(argv: Vec<String>, stdin: Option<&[u8]>) -> (i32, String) {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: Some(PathBuf::from("/")),
        env: Some(vec![
            ("PATH".into(), SUDO_PATH.into()),
            ("LC_ALL".into(), "C".into()),
        ]),
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: if stdin.is_some() {
            StdinMode::Piped
        } else {
            StdinMode::Null
        },
        nice: None,
        address_space_limit_bytes: None,
    });
    if process.start().is_err() {
        return (-1, String::new());
    }
    if let Some(bytes) = stdin {
        // One line, then EOF: sudo-rs would otherwise re-read for a retry.
        let _ = process.write_stdin(bytes);
    }
    let code = process.wait(Some(SUDO_DEADLINE)).unwrap_or_else(|_| {
        let _ = process.kill();
        -1
    });
    let output = process.drain_stream_raw(running_process::StreamKind::Stdout);
    (code, String::from_utf8_lossy(&output).into_owned())
}

/// The leader's sudo state: at most one live ticket for this session.
pub struct Leader {
    sudo: Option<Sudo>,
    ticket: Option<Ticket>,
    keepalive: Duration,
    cap: Duration,
    /// A validation ran, so cleanup must run `sudo -k`.
    used: bool,
}

impl Leader {
    /// `sudo` is `None` when no trustworthy sudo exists: every request then
    /// answers [`Reply::Unavailable`].
    pub fn new(sudo: Option<Sudo>, keepalive: Duration, cap: Duration) -> Self {
        Self {
            sudo,
            ticket: None,
            keepalive,
            cap,
            used: false,
        }
    }

    pub fn handle(&mut self, request: Request, now: Instant) -> Reply {
        let Some(sudo) = &self.sudo else {
            return Reply::Unavailable;
        };
        match request {
            Request::Probe => match classify_run(sudo, &["-n", "-v"], None) {
                Outcome::Ok => Reply::NoPassword,
                Outcome::NeedsPassword => Reply::NeedsPassword,
                Outcome::NoRights | Outcome::Other => Reply::Unavailable,
            },
            Request::Validate(secret) => self.validate(&secret, now),
            Request::Drop => {
                self.revoke();
                Reply::Dropped
            }
        }
    }

    fn validate(&mut self, secret: &[u8], now: Instant) -> Reply {
        let Some(sudo) = &self.sudo else {
            return Reply::Unavailable;
        };
        self.used = true;
        let mut input = Zeroizing::new(Vec::with_capacity(secret.len() + 1));
        input.extend_from_slice(secret);
        input.push(b'\n');
        match classify_run(sudo, &["-S", "-p", "", "-v"], Some(&input)) {
            Outcome::Ok if sudo.shared() => {
                self.ticket = Some(Ticket::new(now));
                let until = SystemTime::now() + self.cap;
                Reply::Active {
                    until: until.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
                }
            }
            Outcome::Ok => {
                // The ticket would not reach the operator's shell (ppid policy
                // or a zero timeout): take it back rather than half-work.
                self.revoke();
                Reply::Unavailable
            }
            Outcome::NoRights => Reply::Unavailable,
            Outcome::NeedsPassword | Outcome::Other => Reply::Wrong,
        }
    }

    /// Run the keep-alive. `Some` when the ticket ended (cap or failed refresh).
    pub fn tick(&mut self, now: Instant) -> Option<Reply> {
        let due = self.ticket.as_ref()?.due(now, self.keepalive, self.cap);
        match due {
            Due::Wait => None,
            Due::Refresh => {
                let refreshed = self
                    .sudo
                    .as_ref()
                    .is_some_and(|sudo| classify_run(sudo, &["-n", "-v"], None) == Outcome::Ok);
                if refreshed {
                    if let Some(ticket) = &mut self.ticket {
                        ticket.refreshed(now);
                    }
                    return None;
                }
                self.revoke();
                Some(Reply::Expired)
            }
            Due::Expire => {
                self.revoke();
                Some(Reply::Expired)
            }
        }
    }

    /// End the ticket: `sudo -k`, which invalidates this session's record
    /// only. Never `-K`, which would also log the user out of sudo in their
    /// own terminals.
    pub fn revoke(&mut self) {
        self.ticket = None;
        if self.used {
            if let Some(sudo) = &self.sudo {
                sudo.run(&["-k"], None);
            }
        }
    }
}

fn classify_run(sudo: &Sudo, args: &[&str], stdin: Option<&[u8]>) -> Outcome {
    let (code, output) = sudo.run(args, stdin);
    classify(code, &output)
}

#[cfg(test)]
#[path = "leader_tests.rs"]
mod tests;

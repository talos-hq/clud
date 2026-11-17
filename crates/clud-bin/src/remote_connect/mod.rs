//! Native client for the clud.bot operator protocol.
pub mod connect;
mod console;
// The leader half runs only where `platform::remote_shell` is supported; on
// hosts with the stub (Windows) only the client half is reachable.
#[allow(
    dead_code,
    reason = "the session-leader half is unused where platform::remote_shell is a stub"
)]
pub(crate) mod leader;
mod mirror;
mod paste;
pub mod proxy;
mod sudo;
mod terminal;
#[cfg(test)]
mod tests;
mod transport;

use serde_json::{json, Value};
use std::io::{BufRead, IsTerminal, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};
use tungstenite::Message;

#[derive(Debug, Clone, clap::Subcommand)]
pub enum Action {
    /// Connect, share the keyword, and approve control in this terminal.
    Connect {
        /// Operator server hostname or HTTPS origin.
        #[arg(default_value = "clud.bot")]
        host: String,
    },
}

pub fn run(action: &Action) -> i32 {
    let Action::Connect { host } = action;
    match run_session(host, None) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("remote: {error}");
            1
        }
    }
}

fn send(socket: &mut transport::Socket, event: Value) -> Result<(), String> {
    socket
        .send(Message::Text(event.to_string().into()))
        .map_err(|_| "remote connection lost".into())
}

fn stdin_lines() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::sync_channel(1);
    // Do not join a blocked stdin reader: operator disconnect and Ctrl+C must exit.
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

fn run_session(host: &str, mode: Option<connect::Mode>) -> Result<i32, String> {
    let base = transport::base_url(host)?;
    let session = transport::create(&base, mode.map(connect::Mode::wire))?;
    println!("Connecting to {}connect#k={}", base, session.keyword);
    std::io::stdout()
        .flush()
        .map_err(|_| "cannot print remote keyword")?;
    let mut socket = match transport::connect(&session) {
        Ok(socket) => socket,
        Err(error) => {
            if let Some(message) = transport::terminal_status(&base, &session.keyword)
                .as_deref()
                .and_then(end_message)
            {
                println!("{message}");
                return Ok(0);
            }
            return Err(error);
        }
    };
    println!(
        "Connected.\n\n  Keyword: {}\n\nRead this keyword to the operator.\n",
        session.keyword
    );
    std::io::stdout()
        .flush()
        .map_err(|_| "cannot print remote keyword")?;
    let stop = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stop);
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))
        .map_err(|_| "cannot install remote cancellation handler")?;
    let inline = answers_inline();
    let input = stdin_lines();
    let mut state = State::new(console::Console::stdout());
    state.inline = inline;
    state.terminal.enable_leader(sudo::advertised(inline));
    state.terminal.configure_proxy(proxy::SessionProxy::new(
        base.as_str(),
        &session.client_token,
        &session.proxy_model,
    )?)?;
    let mut heartbeat = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        state.read_consent(&input, &mut socket)?;
        if let Some(event) = state.sudo.poll(&state.console) {
            send(&mut socket, event)?;
        }
        for _ in 0..32 {
            let Some(output) = state.terminal.output()? else {
                break;
            };
            state.mirror(&output);
            socket.send(output).map_err(|_| "remote connection lost")?;
        }
        if heartbeat.elapsed() >= Duration::from_secs(20) {
            send(&mut socket, json!({"type":"ping"}))?;
            heartbeat = Instant::now();
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                let event: Value =
                    serde_json::from_str(&text).map_err(|_| "invalid remote event")?;
                if state.event(&event, &mut socket)? {
                    return Ok(0);
                }
            }
            Ok(Message::Binary(bytes)) if state.granted && bytes.first() == Some(&1) => {
                let delivery = state.terminal.input(&bytes[1..])?;
                if let Some(notice) = state.input_notice(delivery) {
                    send(&mut socket, notice)?;
                }
            }
            Ok(Message::Close(_)) => return Err("remote connection closed".into()),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return Err("remote connection lost".into()),
            _ => {}
        }
    }
    let _ = socket.close(None);
    Ok(130)
}

/// Whether the answer is typed on the question's own line: the question is
/// drawn on stdout and echoed input comes from stdin, so both must be the
/// terminal. Call it before [`stdin_lines`] starts reading: on Windows,
/// `is_terminal` on a piped stdin queries the pipe's name, which waits behind
/// the reader thread's pending synchronous read, so the client hung until a
/// line arrived and never showed the consent question.
fn answers_inline() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

struct State {
    console: console::Console,
    /// Declared before `terminal`: on drop (Ctrl+C, a lost server) sudo is
    /// dropped before the PTY that holds it is killed.
    sudo: sudo::Sudo,
    terminal: terminal::Terminal,
    connected: bool,
    granted: bool,
    pending: Option<String>,
    approved: Option<String>,
    /// Operator input is being dropped; the operator has been told once.
    dropping: bool,
    /// [`answers_inline`], decided before stdin was being read.
    inline: bool,
}

impl State {
    fn new(console: console::Console) -> Self {
        Self {
            console,
            sudo: sudo::Sudo::new(),
            terminal: terminal::Terminal::default(),
            connected: false,
            granted: false,
            pending: None,
            approved: None,
            dropping: false,
            inline: false,
        }
    }

    /// One notice per run of dropped input frames, none while input flows.
    fn input_notice(&mut self, delivery: terminal::Delivery) -> Option<Value> {
        let dropped = delivery == terminal::Delivery::Dropped;
        let first = dropped && !self.dropping;
        self.dropping = dropped;
        first.then(
            || json!({"type":"pty_error", "pty_id":self.terminal.id(), "code":"input_dropped"}),
        )
    }

    /// The consenting user watches everything the server is sent.
    fn mirror(&self, output: &Message) {
        match output {
            Message::Binary(frame) => self.console.output(&frame[1..]),
            // The only text frame is `pty_exit`: the remote program has ended.
            _ => self.console.settle(),
        }
    }

    fn read_consent(
        &mut self,
        input: &mpsc::Receiver<String>,
        socket: &mut transport::Socket,
    ) -> Result<(), String> {
        let Ok(line) = input.try_recv() else {
            return Ok(());
        };
        if self.sudo.wants_line() {
            if let Some(event) = self.sudo.on_line(line, &self.console) {
                send(socket, event)?;
            }
            return Ok(());
        }
        if let Some(id) = self.pending.take() {
            let allow = crate::yes_no::answer(&line, crate::yes_no::Default::No);
            self.console.answered(self.inline);
            if !allow {
                self.console.line("Remote control declined.");
            }
            self.approved = allow.then(|| id.clone());
            send(
                socket,
                json!({"type":"control_response", "request_id":id, "allow":allow}),
            )?;
        }
        Ok(())
    }

    fn revoke(&mut self) {
        self.sudo.shutdown(&self.console);
        self.granted = false;
        self.pending = None;
        self.approved = None;
        self.terminal.stop();
        self.console.settle();
    }

    /// Open the shell and hand its session leader, if any, to the sudo flow.
    fn open_pty(&mut self, event: &Value) -> Value {
        let failed = json!({"type":"pty_error", "pty_id":event["pty_id"], "code":"spawn_failed"});
        let Ok(opened) = self.terminal.open(event) else {
            return failed;
        };
        if let Some(leader) = self.terminal.take_leader() {
            match sudo::Link::new(leader) {
                Ok(link) => self.sudo.attach(link),
                Err(_) => {
                    self.terminal.stop();
                    return failed;
                }
            }
        }
        opened
    }

    /// `sudo_request` / `sudo_drop`; `false` for any other event.
    fn sudo_event(
        &mut self,
        event: &Value,
        socket: &mut transport::Socket,
    ) -> Result<bool, String> {
        let reply = match event["type"].as_str() {
            Some("sudo_request") => {
                let ready = self.granted && self.terminal.id().is_some();
                self.sudo
                    .on_request(event["request_id"].as_str(), ready, &self.console)
            }
            Some("sudo_drop") => self.sudo.on_drop(&self.console),
            _ => return Ok(false),
        };
        if let Some(reply) = reply {
            send(socket, reply)?;
        }
        Ok(true)
    }

    fn event(&mut self, event: &Value, socket: &mut transport::Socket) -> Result<bool, String> {
        if self.sudo_event(event, socket)? {
            return Ok(false);
        }
        match event["type"].as_str().unwrap_or("") {
            "hello" => {
                self.connected = matches!(event["state"].as_str(), Some("accepted" | "connected"));
                send(socket, caps(self.inline))?;
                if let Some(message) = end_message(event["state"].as_str().unwrap_or("")) {
                    self.console.line(message);
                    return Ok(true);
                }
            }
            "state" if event["state"] == "accepted" => {
                self.connected = true;
                self.console.line("Operator accepted the connection.");
            }
            "control_request" if self.connected && self.pending.is_none() => {
                self.pending = event["request_id"].as_str().map(str::to_owned);
                let question = crate::yes_no::question(
                    "Allow Remote Control by Zach Vorhies?",
                    crate::yes_no::Default::No,
                );
                if self.inline {
                    self.console.prompt(question);
                } else {
                    self.console.line(question.trim_end());
                }
            }
            "control_granted"
                if self.approved.is_some()
                    && event["request_id"].as_str() == self.approved.as_deref() =>
            {
                self.approved = None;
                self.granted = true;
                self.console.line("Remote control granted to Zach Vorhies.");
            }
            "control_revoked" => {
                self.sudo.shutdown(&self.console);
                if let Some(exit) = self.terminal.close_event() {
                    send(socket, exit)?;
                }
                self.revoke();
            }
            "pty_open" if self.granted => send(socket, self.open_pty(event))?,
            "pty_resize" if self.granted && self.terminal.matches(event) => {
                self.terminal.resize(event)?
            }
            "pty_kill" if self.terminal.matches(event) => {
                self.sudo.shutdown(&self.console);
                self.terminal.stop();
                send(
                    socket,
                    json!({"type":"pty_exit", "pty_id":event["pty_id"], "code":null, "signal":null}),
                )?;
            }
            "message" => {
                if let Some(body) = event["body"].as_str() {
                    self.console.line(format!("Operator: {body}"));
                }
            }
            "proxy_ended" => self
                .console
                .line("clud.bot disconnected remote AI inference."),
            "key_request" => {
                eprintln!("Remote key storage is unavailable in this client.");
                send(
                    socket,
                    json!({"type":"key_supplied", "request_id":event["request_id"], "stored":false}),
                )?;
            }
            "disconnect" | "denied" => {
                // Leave a remote full-screen program first, or the notice goes with it.
                self.revoke();
                self.console
                    .line(end_message(event["type"].as_str().unwrap()).unwrap());
                return Ok(true);
            }
            "state" => {
                if let Some(message) = end_message(event["state"].as_str().unwrap_or("")) {
                    self.revoke();
                    self.console.line(message);
                    return Ok(true);
                }
            }
            _ => {}
        }
        Ok(false)
    }
}

/// The capabilities this client advertises. `sudo` only where the shell can
/// run under the session leader and the password can be typed hidden.
fn caps(inline: bool) -> Value {
    let mut caps = vec!["pty"];
    if sudo::advertised(inline) {
        caps.push("sudo");
    }
    json!({"type":"caps", "caps":caps, "client":concat!("clud/", env!("CARGO_PKG_VERSION"))})
}

fn end_message(kind: &str) -> Option<&'static str> {
    match kind {
        "disconnect" | "ended" => Some("clud.bot has ended a connection to this client."),
        "denied" => Some("clud.bot denied this connection."),
        "expired" => Some("This keyword expired. Run `clud remote connect clud.bot` again."),
        _ => None,
    }
}

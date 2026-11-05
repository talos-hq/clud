//! Native client for the clud.bot operator protocol.
mod terminal;
#[cfg(test)]
mod tests;
mod transport;

use serde_json::{json, Value};
use std::io::{BufRead, Write};
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
    match run_session(host) {
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

fn run_session(host: &str) -> Result<i32, String> {
    let base = transport::base_url(host)?;
    let session = transport::create(&base)?;
    println!("Connecting to: {}connect#k={}", base, session.keyword);
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
    println!("Connected!\n\nTell them this keyword:\n{}", session.keyword);
    std::io::stdout()
        .flush()
        .map_err(|_| "cannot print remote keyword")?;
    let stop = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stop);
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))
        .map_err(|_| "cannot install remote cancellation handler")?;
    let input = stdin_lines();
    let mut state = State::default();
    let mut heartbeat = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        state.read_consent(&input, &mut socket)?;
        for _ in 0..32 {
            let Some(output) = state.terminal.output()? else {
                break;
            };
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
                state.terminal.input(&bytes[1..])?;
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

#[derive(Default)]
struct State {
    terminal: terminal::Terminal,
    connected: bool,
    granted: bool,
    pending: Option<String>,
    approved: Option<String>,
}

impl State {
    fn read_consent(
        &mut self,
        input: &mpsc::Receiver<String>,
        socket: &mut transport::Socket,
    ) -> Result<(), String> {
        let Ok(line) = input.try_recv() else {
            return Ok(());
        };
        if let Some(id) = self.pending.take() {
            let allow = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
            self.approved = allow.then(|| id.clone());
            send(
                socket,
                json!({"type":"control_response", "request_id":id, "allow":allow}),
            )?;
        }
        Ok(())
    }

    fn revoke(&mut self) {
        self.granted = false;
        self.pending = None;
        self.approved = None;
        self.terminal.stop();
    }

    fn event(&mut self, event: &Value, socket: &mut transport::Socket) -> Result<bool, String> {
        match event["type"].as_str().unwrap_or("") {
            "hello" => {
                self.connected = matches!(event["state"].as_str(), Some("accepted" | "connected"));
                send(
                    socket,
                    json!({"type":"caps", "caps":["pty"], "client":concat!("clud/", env!("CARGO_PKG_VERSION"))}),
                )?;
                if let Some(message) = end_message(event["state"].as_str().unwrap_or("")) {
                    println!("{message}");
                    return Ok(true);
                }
            }
            "state" if event["state"] == "accepted" => {
                self.connected = true;
                println!("Operator accepted the connection.");
            }
            "control_request" if self.connected && self.pending.is_none() => {
                self.pending = event["request_id"].as_str().map(str::to_owned);
                println!("Allow Remote Control by Zach Vorhies? [y/N]");
                std::io::stdout()
                    .flush()
                    .map_err(|_| "cannot print consent prompt")?;
            }
            "control_granted"
                if self.approved.is_some()
                    && event["request_id"].as_str() == self.approved.as_deref() =>
            {
                self.approved = None;
                self.granted = true;
                println!("Remote control granted to Zach Vorhies.");
            }
            "control_revoked" => {
                if let Some(exit) = self.terminal.close_event() {
                    send(socket, exit)?;
                }
                self.revoke();
            }
            "pty_open" if self.granted => {
                let response = self.terminal.open(event).unwrap_or_else(|_| json!({"type":"pty_error", "pty_id":event["pty_id"], "code":"spawn_failed"}));
                send(socket, response)?;
            }
            "pty_resize" if self.granted && self.terminal.matches(event) => {
                self.terminal.resize(event)?
            }
            "pty_kill" if self.terminal.matches(event) => {
                self.terminal.stop();
                send(
                    socket,
                    json!({"type":"pty_exit", "pty_id":event["pty_id"], "code":null, "signal":null}),
                )?;
            }
            "message" => {
                if let Some(body) = event["body"].as_str() {
                    println!("Operator: {body}");
                }
            }
            "proxy_ended" => println!("clud.bot disconnected remote AI inference."),
            "key_request" => {
                eprintln!("Remote key storage is unavailable in this client.");
                send(
                    socket,
                    json!({"type":"key_supplied", "request_id":event["request_id"], "stored":false}),
                )?;
            }
            "disconnect" | "denied" => {
                println!("{}", end_message(event["type"].as_str().unwrap()).unwrap());
                self.revoke();
                return Ok(true);
            }
            "state" => {
                if let Some(message) = end_message(event["state"].as_str().unwrap_or("")) {
                    println!("{message}");
                    self.revoke();
                    return Ok(true);
                }
            }
            _ => {}
        }
        Ok(false)
    }
}

fn end_message(kind: &str) -> Option<&'static str> {
    match kind {
        "disconnect" | "ended" => Some("clud.bot has ended a connection to this client."),
        "denied" => Some("clud.bot denied this connection."),
        "expired" => Some("This keyword expired. Run `clud remote connect clud.bot` again."),
        _ => None,
    }
}

//! Native client for the clud.bot operator protocol.
pub mod connect;
mod console;
mod paste;
pub mod proxy;
mod terminal;
#[cfg(test)]
mod tests;
mod transport;

use crate::platform::console_echo::{suppress_stdin_echo, StdinEchoGuard};
use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
};
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
    let mut state = State::new(console::Console::stdout());
    state.terminal.configure_proxy(proxy::SessionProxy::new(
        base.as_str(),
        &session.client_token,
        &session.proxy_model,
    )?)?;
    let mut heartbeat = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        state.read_consent(&input, &mut socket)?;
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

/// An operator sudo request awaiting the user's local decision, then password.
struct PendingSudo {
    request_id: String,
    command: String,
    /// False while the local `[y/N]` is pending, true once the user consented
    /// and the client is waiting for the password line.
    awaiting_password: bool,
    /// Holds the echo-suppression guard while the password is typed, so it is
    /// never shown on screen or transmitted. `None` if the terminal could not
    /// be switched (the password still never leaves the machine).
    echo: Option<StdinEchoGuard>,
}

struct State {
    console: console::Console,
    terminal: terminal::Terminal,
    connected: bool,
    granted: bool,
    pending: Option<String>,
    approved: Option<String>,
    pending_sudo: Option<PendingSudo>,
}

impl State {
    fn new(console: console::Console) -> Self {
        Self {
            console,
            terminal: terminal::Terminal::default(),
            connected: false,
            granted: false,
            pending: None,
            approved: None,
            pending_sudo: None,
        }
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
        // A sudo request takes the line first: the server only sends one once
        // control is granted, so a control `[y/N]` is never also pending.
        if self.pending_sudo.is_some() {
            return self.advance_sudo(line, socket);
        }
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

    /// Drive a pending sudo request one input line at a time: first the local
    /// `[y/N]`, then the password (read with echo suppressed, never sent).
    fn advance_sudo(&mut self, line: String, socket: &mut transport::Socket) -> Result<(), String> {
        let Some(mut sudo) = self.pending_sudo.take() else {
            return Ok(());
        };
        if !sudo.awaiting_password {
            // The consent line.
            if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                self.console.line("Root command denied.");
                return send(
                    socket,
                    json!({"type":"sudo_result", "request_id":sudo.request_id, "status":"denied"}),
                );
            }
            self.console
                .line("[sudo] password (typed here, hidden from the operator):");
            // Suppress echo while the password is typed. On failure the password
            // still never leaves the machine; it just is not hidden on screen.
            sudo.echo = suppress_stdin_echo().ok();
            sudo.awaiting_password = true;
            self.pending_sudo = Some(sudo);
            return Ok(());
        }
        // The password line. Drop the guard first to restore echo.
        let password = line.strip_suffix('\n').unwrap_or(&line).to_owned();
        drop(sudo.echo.take());
        self.console.line("");
        let code = run_sudo(socket, &self.console, &sudo.command, &password);
        self.console
            .line(format!("Root command exited with status {code}."));
        send(
            socket,
            json!({"type":"sudo_result", "request_id":sudo.request_id, "status":"ran", "exit_code":code}),
        )
    }

    /// Open an operator-supplied address in the user's browser. Silent, but the
    /// client refuses anything that is not https (or loopback) and always reports
    /// what it opened, so a drive-by `file:`/`http:` is blocked and nothing opens
    /// invisibly.
    fn handle_open_url(&mut self, event: &Value) {
        match sanitize_open_url(event["url"].as_str().unwrap_or("")) {
            Ok(url) => {
                if open::that_detached(&url).is_ok() {
                    self.console
                        .line(format!("clud.bot opened in your browser: {url}"));
                } else {
                    self.console.line(format!("clud.bot could not open: {url}"));
                }
            }
            Err(reason) => self
                .console
                .line(format!("clud.bot refused to open an address ({reason}).")),
        }
    }

    /// Park an operator sudo request for the local `[y/N]` + password flow. The
    /// password is read locally later and never transmitted. Control is already
    /// granted (the server only sends this then).
    fn begin_sudo(&mut self, event: &Value) {
        let request_id = event["request_id"].as_str().unwrap_or("").to_owned();
        let command = event["command"].as_str().unwrap_or("").to_owned();
        self.console.line(format!(
            "Allow a ROOT command by Zach Vorhies? [y/N]\n  sudo {command}"
        ));
        self.pending_sudo = Some(PendingSudo {
            request_id,
            command,
            awaiting_password: false,
            echo: None,
        });
    }

    fn revoke(&mut self) {
        self.granted = false;
        self.pending = None;
        self.approved = None;
        self.pending_sudo = None;
        self.terminal.stop();
        self.console.settle();
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
                self.console
                    .line("Allow Remote Control by Zach Vorhies? [y/N]");
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
                    self.console.line(format!("Operator: {body}"));
                }
            }
            "open_url" if self.connected => self.handle_open_url(event),
            "sudo_request" if self.granted && self.pending_sudo.is_none() => self.begin_sudo(event),
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

/// Accept only an address the client is willing to open in the user's browser:
/// `https://` with a host, or a loopback `http://` origin for local development.
/// Everything else (`file:`, `javascript:`, `data:`, bare paths, remote `http://`)
/// is refused so an operator cannot open a local file or an unencrypted page.
fn sanitize_open_url(raw: &str) -> Result<String, &'static str> {
    let url = raw.trim();
    if url.len() > 2048 {
        return Err("address too long");
    }
    if let Some(rest) = url.strip_prefix("https://") {
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host = host.rsplit('@').next().unwrap_or("");
        if host.is_empty() {
            return Err("missing host");
        }
        return Ok(url.to_owned());
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host = host.rsplit('@').next().unwrap_or("");
        let name = if let Some(inside) = host.strip_prefix('[') {
            inside.split(']').next().unwrap_or("")
        } else {
            host.split(':').next().unwrap_or("")
        };
        if matches!(name, "localhost" | "127.0.0.1" | "::1") {
            return Ok(url.to_owned());
        }
        return Err("only https or loopback http is allowed");
    }
    Err("only https addresses are allowed")
}

/// Most output a sudo command may stream to the operator before it is dropped.
const SUDO_OUTPUT_CAP: usize = 1024 * 1024;

/// Run `sudo -S <command>` locally, feeding `password` on stdin (never over the
/// wire) and streaming the merged output to the operator (binary channel 0) and
/// the local console. Returns the exit status. `-k` ignores any cached
/// credential so a wrong password fails closed instead of silently succeeding.
fn run_sudo(
    socket: &mut transport::Socket,
    console: &console::Console,
    command: &str,
    password: &str,
) -> i32 {
    let argv: Vec<String> = vec![
        "sudo".into(),
        "-S".into(),
        "-k".into(),
        "-p".into(),
        String::new(),
        "--".into(),
        "sh".into(),
        "-c".into(),
        command.into(),
    ];
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Piped,
        nice: None,
        address_space_limit_bytes: None,
    });
    if process.start().is_err() {
        console.line("Could not launch sudo on this machine.");
        return 127;
    }
    let _ = process.write_stdin(format!("{password}\n").as_bytes());
    let _ = process.close_stdin();
    let mut sent = 0usize;
    let mut last_ping = Instant::now();
    loop {
        match process.read_combined(Some(Duration::from_millis(200))) {
            ReadStatus::Line(event) if sent < SUDO_OUTPUT_CAP => {
                let mut frame = Vec::with_capacity(event.line.len() + 1);
                frame.push(0);
                frame.extend_from_slice(&event.line);
                let _ = socket.send(Message::Binary(frame.into()));
                console.output(&event.line);
                sent += event.line.len();
            }
            ReadStatus::Line(_) => {}
            ReadStatus::Timeout => {
                // Keep the server's liveness check happy during a long command.
                if last_ping.elapsed() >= Duration::from_secs(15) {
                    let _ = send(socket, json!({"type":"ping"}));
                    last_ping = Instant::now();
                }
            }
            ReadStatus::Eof => break,
        }
    }
    process.wait(Some(Duration::from_secs(5))).unwrap_or(1)
}

fn end_message(kind: &str) -> Option<&'static str> {
    match kind {
        "disconnect" | "ended" => Some("clud.bot has ended a connection to this client."),
        "denied" => Some("clud.bot denied this connection."),
        "expired" => Some("This keyword expired. Run `clud remote connect clud.bot` again."),
        _ => None,
    }
}

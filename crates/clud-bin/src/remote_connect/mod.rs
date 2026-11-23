//! Native client for the clud.bot operator protocol.
mod bridge;
pub mod connect;
mod console;
mod dns;
mod e2e;
// The leader half runs only where `platform::remote_shell` is supported; on
// hosts with the stub (Windows) only the client half is reachable.
#[allow(
    dead_code,
    reason = "the session-leader half is unused where platform::remote_shell is a stub"
)]
pub(crate) mod leader;
mod mirror;
mod open_url;
mod paste;
pub mod proxy;
mod resume;
mod size;
mod sudo;
mod terminal;
#[cfg(test)]
mod tests;
mod tls;
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

/// A failed write on the session socket. Callers compare against it to tell a
/// lost connection (reconnect) from a failed session (exit).
const CONNECTION_LOST: &str = "remote connection lost";

fn send(socket: &mut transport::Socket, event: Value) -> Result<(), String> {
    socket
        .send(Message::Text(event.to_string().into()))
        .map_err(|_| CONNECTION_LOST.into())
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
    let mut session = transport::create(&base, mode.map(connect::Mode::wire))?;
    println!("Connecting to {}connect#k={}", base, session.keyword);
    std::io::stdout()
        .flush()
        .map_err(|_| "cannot print remote keyword")?;
    let socket = match transport::connect(&mut session) {
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
    state.size = size::SessionSize::watch_stdout();
    state.terminal.enable_leader(sudo::advertised(inline));
    // Configured on `hello`, which says whether model requests ride the socket.
    state.proxy = Some(proxy::SessionProxy::new(
        base.as_str(),
        &session.client_token,
        &session.proxy_model,
    )?);
    let link = Link {
        base,
        session,
        input,
        stop,
    };
    link.run(&mut state, socket)
}

/// How a connected phase ended.
enum Online {
    Exit(i32),
    /// The connection went away; `restart` when the server announced it.
    Lost {
        restart: bool,
    },
}

/// What a session needs to outlive one connection.
struct Link {
    base: url::Url,
    session: transport::Session,
    input: mpsc::Receiver<String>,
    stop: Arc<AtomicBool>,
}

impl Link {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Alternate connected and reconnecting phases until the session ends.
    fn run(&self, state: &mut State, mut socket: transport::Socket) -> Result<i32, String> {
        loop {
            let outcome = self.online(state, &mut socket);
            let restart = match outcome {
                Ok(Online::Exit(code)) => {
                    leave(&mut socket);
                    return Ok(code);
                }
                Ok(Online::Lost { restart }) => restart,
                Err(error) => {
                    leave(&mut socket);
                    return Err(error);
                }
            };
            // A server without resume support ended the session with the socket.
            if !state.resumes {
                return Err(CONNECTION_LOST.into());
            }
            match self.offline(state, restart)? {
                Ok(next) => socket = next,
                Err(code) => return Ok(code),
            }
        }
    }

    /// One connection: forward output, answer the server, watch the heartbeat.
    /// The socket never blocks: a pass that finds nothing to do sleeps briefly
    /// ([`transport::Idle`]), and a congested link is waited out here, with
    /// the shell paced to it, until the heartbeat says the link is gone.
    fn online(&self, state: &mut State, socket: &mut transport::Socket) -> Result<Online, String> {
        let lost = Ok(Online::Lost { restart: false });
        let mut ping = Instant::now();
        let mut heard = Instant::now();
        let mut idle = transport::Idle::new();
        while !self.stopped() {
            if socket.pump().is_err() {
                return lost;
            }
            if let Some(size) = state.finish_repaint()? {
                if lost_on(send(socket, size))? {
                    return lost;
                }
            }
            let Some(mut active) = self.send_local(state, socket)? else {
                return lost;
            };
            match state.forward(socket)? {
                Some(forwarded) => active |= forwarded,
                None => return lost,
            }
            match bridge::flush(state.bridge.as_ref(), socket) {
                Ok(flushed) => active |= flushed,
                Err(error) if error == CONNECTION_LOST => return lost,
                Err(error) => return Err(error),
            }
            if ping.elapsed() >= resume::HEARTBEAT {
                ping = Instant::now();
                if lost_on(send(socket, json!({"type":"ping"})))? {
                    return lost;
                }
            }
            // No pong for several pings: a half-open connection TCP has not noticed.
            if heard.elapsed() >= resume::HEARTBEAT_TIMEOUT {
                return lost;
            }
            match socket.read() {
                Ok(message) => {
                    heard = Instant::now();
                    active = true;
                    if let Some(outcome) = state.receive(message, socket)? {
                        return Ok(outcome);
                    }
                }
                Err(tungstenite::Error::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return lost,
            }
            idle.after(active);
        }
        Ok(Online::Exit(130))
    }

    /// Send what the local side produced: a consent answer, sudo's state and a
    /// local resize. `Some(active)`, or `None` when the connection is lost.
    fn send_local(
        &self,
        state: &mut State,
        socket: &mut transport::Socket,
    ) -> Result<Option<bool>, String> {
        let local = state.read_consent(&self.input);
        let resized = state.local_resize()?;
        let mut active = false;
        for event in local
            .into_iter()
            .chain(state.sudo.poll(&state.console))
            .chain(resized)
        {
            active = true;
            if lost_on(send(socket, event.clone()))? {
                state.hold_event(event);
                return Ok(None);
            }
        }
        Ok(Some(active))
    }

    /// Disconnected: keep the shell alive and reconnect within the window.
    /// `Ok(Ok(socket))` resumed, `Ok(Err(code))` the session is over.
    fn offline(
        &self,
        state: &mut State,
        restart: bool,
    ) -> Result<Result<transport::Socket, i32>, String> {
        state.disconnected();
        let mut reconnector = resume::Reconnector::new(&self.base, &self.session, restart);
        loop {
            if self.stopped() {
                return Ok(Err(130));
            }
            let local = state.read_consent(&self.input);
            // The PTY follows the local window now; the server hears on resume.
            let resized = state.local_resize()?;
            for event in local
                .into_iter()
                .chain(state.sudo.poll(&state.console))
                .chain(resized)
            {
                state.hold_event(event);
            }
            // The size is announced again on resume.
            let _ = state.finish_repaint()?;
            let busy = state.hold_output()?;
            match reconnector.poll() {
                resume::Poll::Connected(socket) => return Ok(Ok(*socket)),
                resume::Poll::Ended(message) => {
                    state.revoke();
                    state.console.line(message);
                    return Ok(Err(0));
                }
                resume::Poll::GaveUp => return Err(resume::GAVE_UP.into()),
                resume::Poll::Insecure(message) => {
                    state.revoke();
                    return Err(message);
                }
                resume::Poll::Waiting if busy => {}
                resume::Poll::Waiting => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}

/// `Ok(true)` when `result` is the lost-connection error; other errors stay errors.
fn lost_on(result: Result<(), String>) -> Result<bool, String> {
    match result {
        Ok(()) => Ok(false),
        Err(error) if error == CONNECTION_LOST => Ok(true),
        Err(error) => Err(error),
    }
}

/// Close deliberately (1000): the server ends the session now instead of
/// holding it for a reconnect that will not come.
fn leave(socket: &mut transport::Socket) {
    socket.close_within(
        tungstenite::protocol::CloseFrame {
            code: tungstenite::protocol::frame::coding::CloseCode::Normal,
            reason: "".into(),
        },
        LEAVE_WAIT,
    );
}

/// How long a deliberate close may wait for a slow link to carry it.
const LEAVE_WAIT: Duration = Duration::from_secs(2);

/// A PTY shrunk by a row to make the remote program repaint.
struct Repaint {
    due: Instant,
    nudged: (u16, u16),
    before: (u16, u16),
}

/// How long the PTY stays a row shorter to make the remote program repaint
/// after the connection banner is cleared ([`State::start_repaint`]): long
/// enough that a program reading its size on the resize signal sees the
/// change, short enough to go unnoticed.
const REPAINT_HOLD: Duration = Duration::from_millis(200);

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
    /// The last `sudo_state` the server did not get, sent when it resumes.
    sudo_held: Option<Value>,
    terminal: terminal::Terminal,
    /// Whose terminal sets the PTY's size (`size.rs`).
    size: size::SessionSize,
    /// The agent's model route, configured on the first `hello`.
    proxy: Option<proxy::SessionProxy>,
    /// Model requests on the session socket, when `hello` offers channel 2.
    bridge: Option<bridge::Bridge>,
    connected: bool,
    granted: bool,
    /// The request the user's grant answered; a resumed session keeps control
    /// only if the server still records this same grant.
    grant_id: Option<String>,
    pending: Option<String>,
    approved: Option<String>,
    /// The user's last answer and its request, until the server settles it:
    /// an answer typed while disconnected is sent when the session resumes.
    answer: Option<(String, bool)>,
    /// Operator input is being dropped; the operator has been told once.
    dropping: bool,
    /// [`answers_inline`], decided before stdin was being read.
    inline: bool,
    /// The server keeps this session through a lost socket (`hello.resume`).
    resumes: bool,
    /// Reconnected; the next `hello` is reconciled against local consent.
    resuming: bool,
    /// Shell output produced while disconnected.
    outage: resume::Outage,
    /// A repaint nudge in progress ([`State::start_repaint`]).
    repaint: Option<Repaint>,
}

impl State {
    fn new(console: console::Console) -> Self {
        Self {
            console,
            sudo: sudo::Sudo::new(),
            sudo_held: None,
            terminal: terminal::Terminal::default(),
            size: size::SessionSize::default(),
            proxy: None,
            bridge: None,
            connected: false,
            granted: false,
            grant_id: None,
            pending: None,
            approved: None,
            answer: None,
            dropping: false,
            inline: false,
            resumes: false,
            resuming: false,
            outage: resume::Outage::default(),
            repaint: None,
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

    /// The user's answer to the open consent question, as the response to
    /// send. It is remembered either way, so one typed while the connection
    /// is down still reaches the server when the session resumes.
    fn read_consent(&mut self, input: &mpsc::Receiver<String>) -> Option<Value> {
        let line = input.try_recv().ok()?;
        if self.sudo.wants_line() {
            return self.sudo.on_line(line, &self.console);
        }
        let id = self.pending.take()?;
        let allow = crate::yes_no::answer(&line, crate::yes_no::Default::No);
        self.console.answered(self.inline);
        if !allow {
            self.console.line("Remote control declined.");
        }
        self.approved = allow.then(|| id.clone());
        self.answer = Some((id.clone(), allow));
        Some(control_response(&id, allow))
    }

    fn ask(&mut self, id: &str) {
        self.pending = Some(id.to_owned());
        self.answer = None;
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

    fn revoke(&mut self) {
        self.sudo.shutdown(&self.console);
        self.granted = false;
        self.grant_id = None;
        self.pending = None;
        self.approved = None;
        self.answer = None;
        self.outage.discard();
        self.terminal.stop();
        self.console.settle();
    }

    /// An event the server could not be sent. A consent answer is already
    /// remembered in `answer`; only the latest sudo state needs keeping.
    fn hold_event(&mut self, event: Value) {
        if event["type"] == "sudo_state" {
            self.sudo_held = Some(event);
        }
    }

    /// Tell the server where sudo stands: a restarted server has forgotten,
    /// and a state reached while disconnected never reached it.
    fn sync_sudo(&mut self, record: &Value, socket: &mut transport::Socket) -> Result<(), String> {
        let held = self.sudo_held.take();
        let report = self.sudo.report().or(held).or_else(|| {
            // Nothing is running here, but the server still shows sudo on.
            matches!(record["state"].as_str(), Some("prompting" | "active"))
                .then(|| sudo::state_event(record["request_id"].as_str(), sudo::Wire::Dropped))
        });
        match report {
            Some(event) => send(socket, event),
            None => Ok(()),
        }
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

    /// `pty_open` / `pty_resize` / `pty_kill`; `false` for any other event.
    /// While the local terminal sets the size ([`size::SessionSize`]), the
    /// operator's size is replaced by it and the actual size is announced.
    fn pty_event(&mut self, event: &Value, socket: &mut transport::Socket) -> Result<bool, String> {
        match event["type"].as_str() {
            Some("pty_open") if self.granted => {
                let event = self.size.applied(event);
                send(socket, self.open_pty(&event))?;
                self.announce_size(socket)?;
            }
            Some("pty_resize") if self.granted && self.terminal.matches(event) => {
                if self.size.governing().is_some() {
                    self.announce_size(socket)?;
                } else {
                    self.terminal.resize(event)?;
                }
            }
            Some("pty_kill") if self.terminal.matches(event) => {
                self.sudo.shutdown(&self.console);
                self.terminal.stop();
                send(
                    socket,
                    json!({"type":"pty_exit", "pty_id":event["pty_id"], "code":null, "signal":null}),
                )?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Tell a server that follows the client's size what the PTY's size is.
    fn announce_size(&self, socket: &mut transport::Socket) -> Result<(), String> {
        match self.size.governing().and(self.terminal.size_event()) {
            Some(event) => send(socket, event),
            None => Ok(()),
        }
    }

    /// Resize the PTY to the local terminal's new size, when that sets the
    /// session's size; the `pty_size` to announce.
    fn local_resize(&mut self) -> Result<Option<Value>, String> {
        let Some(size) = self.size.changed() else {
            return Ok(None);
        };
        self.terminal.resize_to(size)?;
        Ok(self.terminal.size_event())
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

    /// Forward waiting shell output while the network takes it: output
    /// still held from an outage first, then the shell's. While the socket is
    /// backed up the shell is not read, so it waits at the link's pace and the
    /// queue in front of a heartbeat stays short. `Some(active)`, or `None`
    /// when the connection is lost (the frame that failed is held).
    fn forward(&mut self, socket: &mut transport::Socket) -> Result<Option<bool>, String> {
        // Reconnected but not reconciled: nothing of the shell's leaves until
        // the server's `hello` confirms this client still holds the grant.
        if self.resuming {
            return Ok(Some(false));
        }
        let Some(mut active) = self.drain_outage(socket) else {
            return Ok(None);
        };
        if !self.outage.is_empty() {
            return Ok(Some(active));
        }
        for _ in 0..terminal::OUTPUT_DRAIN {
            if socket.backed_up() {
                break;
            }
            let Some(output) = self.terminal.output()? else {
                break;
            };
            active = true;
            self.mirror(&output);
            if socket.send(output.clone()).is_err() {
                self.outage.push(output);
                return Ok(None);
            }
        }
        Ok(Some(active))
    }

    /// Send held output until the network is full; the rest stays held, in
    /// front of anything newer. `Some(sent)`, or `None` when the connection is
    /// lost.
    fn drain_outage(&mut self, socket: &mut transport::Socket) -> Option<bool> {
        let mut sent = false;
        while !socket.backed_up() {
            if let Some(gap) = self.outage.gap() {
                send(socket, gap).ok()?;
                self.outage.gap_sent();
                sent = true;
                continue;
            }
            let Some(frame) = self.outage.front() else {
                break;
            };
            socket.send(frame.clone()).ok()?;
            self.outage.frame_sent();
            sent = true;
        }
        self.outage.clear_drained_pty();
        Some(sent)
    }

    /// One frame from the server; `Some` ends this connection's phase.
    fn receive(
        &mut self,
        message: Message,
        socket: &mut transport::Socket,
    ) -> Result<Option<Online>, String> {
        let lost = Some(Online::Lost { restart: false });
        match message {
            Message::Text(text) => {
                let event: Value =
                    serde_json::from_str(&text).map_err(|_| "invalid remote event")?;
                if event["type"] == "server_restart" {
                    return Ok(Some(Online::Lost { restart: true }));
                }
                match self.event(&event, socket) {
                    Ok(true) => Ok(Some(Online::Exit(0))),
                    Ok(false) => Ok(None),
                    Err(error) if error == CONNECTION_LOST => Ok(lost),
                    Err(error) => Err(error),
                }
            }
            Message::Binary(bytes) if bytes.first() == Some(&bridge::CHANNEL) => {
                if let Some(bridge) = &self.bridge {
                    bridge.deliver(&bytes)?;
                }
                Ok(None)
            }
            Message::Binary(bytes) if self.granted && bytes.first() == Some(&1) => {
                let delivery = self.terminal.input(&bytes[1..])?;
                let Some(notice) = self.input_notice(delivery) else {
                    return Ok(None);
                };
                Ok(if lost_on(send(socket, notice))? {
                    lost
                } else {
                    None
                })
            }
            Message::Close(frame) => match resume::close_meaning(frame.as_ref()) {
                resume::Close::Lost => Ok(lost),
                resume::Close::Final(message) => {
                    self.revoke();
                    Err(message.into())
                }
            },
            _ => Ok(None),
        }
    }

    /// The connection is gone: say so once, and hold output from here on.
    /// The remote program keeps running and keeps drawing on this terminal,
    /// so its modes stay as they are and the notice is a banner that the
    /// resume clears again (`Console::banner`).
    fn disconnected(&mut self) {
        self.console.banner(resume::LOST_NOTICE);
        self.outage.begin(self.terminal.report());
        if let Some(bridge) = &self.bridge {
            bridge.reset();
        }
        self.resuming = true;
    }

    /// While disconnected: keep reading the shell, so it never blocks on a full
    /// PTY, mirror it locally and hold it (bounded) for the operator.
    /// `true` while output keeps coming, so the caller does not sleep.
    fn hold_output(&mut self) -> Result<bool, String> {
        for _ in 0..terminal::OUTPUT_DRAIN {
            let Some(output) = self.terminal.output()? else {
                return Ok(false);
            };
            self.mirror(&output);
            if self.granted {
                self.outage.push(output);
            }
        }
        Ok(true)
    }

    /// Reconcile with the server's record after a reconnect (`hello.resume`).
    /// Local consent stays authoritative: control continues only if this
    /// client still holds the very grant the server records, and is never
    /// gained without the user's own yes.
    fn resume(&mut self, hello: &Value, socket: &mut transport::Socket) -> Result<(), String> {
        let record = &hello["resume"];
        let control = record["control"].as_str().unwrap_or("none");
        let grant = record["grant_id"].as_str().filter(|_| control == "granted");
        self.reconcile_grant(grant);
        if control == "granted" && !self.granted {
            send(socket, json!({"type":"control_release"}))?;
        }
        let request = record["control_request"]
            .as_str()
            .filter(|_| control == "requested");
        self.reconcile_request(request, socket)?;
        let pty = if self.granted {
            self.terminal.report().or_else(|| self.outage.pty())
        } else {
            None
        };
        send(socket, json!({"type":"resume", "pty":pty}))?;
        self.announce_size(socket)?;
        self.sync_sudo(&record["sudo"], socket)?;
        self.flush_outage(socket)?;
        if self.console.clear_banner(resume::RESUMED_NOTICE) {
            self.start_repaint()?;
        }
        Ok(())
    }

    /// The banner covered part of the remote program's screen: shrink the PTY
    /// by a row now and restore it after [`REPAINT_HOLD`], which makes the
    /// program draw its whole screen again (and ConPTY re-emit it).
    fn start_repaint(&mut self) -> Result<(), String> {
        let Some(before @ (rows, cols)) = self.terminal.size() else {
            return Ok(());
        };
        if rows < 2 {
            return Ok(());
        }
        let nudged = (rows - 1, cols);
        self.terminal.resize_to(nudged)?;
        self.repaint = Some(Repaint {
            due: Instant::now() + REPAINT_HOLD,
            nudged,
            before,
        });
        Ok(())
    }

    /// Give the PTY its size back once a repaint nudge has been seen: the
    /// local terminal's current size when it sets the session's, else the
    /// size before the nudge. A resize that arrived meanwhile already set the
    /// size and is kept. The `pty_size` to announce, when this client sets it.
    fn finish_repaint(&mut self) -> Result<Option<Value>, String> {
        let Some(repaint) = self
            .repaint
            .take_if(|repaint| Instant::now() >= repaint.due)
        else {
            return Ok(None);
        };
        if self.terminal.size() != Some(repaint.nudged) {
            return Ok(None);
        }
        let governing = self.size.governing();
        self.terminal
            .resize_to(governing.unwrap_or(repaint.before))?;
        // A size announced during the nudge was the nudged one.
        Ok(governing.and(self.terminal.size_event()))
    }

    fn reconcile_grant(&mut self, grant: Option<&str>) {
        if self.granted {
            if grant.is_none() || grant != self.grant_id.as_deref() {
                self.revoke();
                self.console
                    .line("Remote control ended while the connection was down.");
            }
        } else if grant.is_some() && grant == self.approved.as_deref() {
            // The user said yes and the server granted it; only the notice was lost.
            self.approved = None;
            self.answer = None;
            self.granted = true;
            self.grant_id = grant.map(str::to_owned);
            self.console.line("Remote control granted to Zach Vorhies.");
        }
    }

    fn reconcile_request(
        &mut self,
        request: Option<&str>,
        socket: &mut transport::Socket,
    ) -> Result<(), String> {
        let Some(id) = request else {
            self.approved = None;
            self.answer = None;
            if self.pending.take().is_some() {
                self.console.answered(false);
                self.console
                    .line("The operator withdrew the control request.");
            }
            return Ok(());
        };
        match &self.answer {
            // Answered while disconnected (or the answer was lost): send it now.
            Some((answered, allow)) if answered == id => send(socket, control_response(id, *allow)),
            // Still on screen and unanswered: never ask twice.
            _ if self.pending.as_deref() == Some(id) => Ok(()),
            _ => {
                self.ask(id);
                Ok(())
            }
        }
    }

    /// Send output held while disconnected, after a gap notice if some was dropped.
    fn flush_outage(&mut self, socket: &mut transport::Socket) -> Result<(), String> {
        if !self.granted {
            self.outage.discard();
            return Ok(());
        }
        match self.drain_outage(socket) {
            Some(_) => Ok(()),
            None => Err(CONNECTION_LOST.into()),
        }
    }

    fn event(&mut self, event: &Value, socket: &mut transport::Socket) -> Result<bool, String> {
        if self.sudo_event(event, socket)? || self.pty_event(event, socket)? {
            return Ok(false);
        }
        match event["type"].as_str().unwrap_or("") {
            "hello" => {
                self.size.hello(event);
                self.connected = matches!(event["state"].as_str(), Some("accepted" | "connected"));
                self.resumes = event["resume"].is_object();
                send(socket, caps(self.inline))?;
                if let Some(mut proxy) = self.proxy.take() {
                    self.bridge = bridge::offer(event, &mut proxy)?;
                    self.terminal.configure_proxy(proxy)?;
                }
                if let Some(message) = end_message(event["state"].as_str().unwrap_or("")) {
                    self.revoke();
                    self.console.line(message);
                    return Ok(true);
                }
                if std::mem::take(&mut self.resuming) {
                    self.resume(event, socket)?;
                }
            }
            "state" if event["state"] == "accepted" => {
                self.connected = true;
                self.console.line("Operator accepted the connection.");
            }
            "control_request" if self.connected && self.pending.is_none() => {
                if let Some(id) = event["request_id"].as_str() {
                    self.ask(id);
                }
            }
            "control_granted"
                if self.approved.is_some()
                    && event["request_id"].as_str() == self.approved.as_deref() =>
            {
                self.grant_id = self.approved.take();
                self.answer = None;
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
            "message" => {
                if let Some(body) = event["body"].as_str() {
                    self.console.line(format!("Operator: {body}"));
                }
            }
            "open_url" => open_url::handle(
                event,
                self.connected && self.granted,
                &self.console,
                |url| open::that_detached(url).is_ok(),
            ),
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

/// The capabilities this client advertises: `resume` always, `sudo` only
/// where the shell can run under the session leader and the password can be
/// typed hidden.
fn caps(inline: bool) -> Value {
    let mut caps = vec!["pty", "resume"];
    if sudo::advertised(inline) {
        caps.push("sudo");
    }
    json!({"type":"caps", "caps":caps, "client":concat!("clud/", env!("CARGO_PKG_VERSION"))})
}

fn control_response(id: &str, allow: bool) -> Value {
    json!({"type":"control_response", "request_id":id, "allow":allow})
}

fn end_message(kind: &str) -> Option<&'static str> {
    match kind {
        "disconnect" | "ended" => Some("clud.bot has ended a connection to this client."),
        "denied" => Some("clud.bot denied this connection."),
        "expired" => Some("This keyword expired. Run `clud remote connect clud.bot` again."),
        _ => None,
    }
}

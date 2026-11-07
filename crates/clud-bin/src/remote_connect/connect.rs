//! `clud connect`: the user chooses what the operator's server may do here.
//!
//! Full control is the consent-gated shell of `clud remote connect`. Proxy opens
//! no shell at all: it asks the operator to approve inference for this session,
//! then runs the user's own agent through that session's bearer.
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::Message;

use crate::selector::{self, Key, Note, Row, Selector, Step, View};
use crate::subprocess::ManagedSubprocess;

use super::{proxy, transport};

const MODEL_RESERVED: &str = "clud connect: --model is reserved for future use and is disabled; \
                              the server chooses the proxied model";
const CHOICES: [(Mode, &str); 2] = [
    (Mode::FullControl, "Allow Full Control"),
    (Mode::Proxy, "Use Proxy"),
];
const PROMPT: &str = "Make your selection: ";
const APPROVED: &str = "Proxy approved. Starting your agent; it will ask before running tools.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    FullControl,
    Proxy,
}

impl Mode {
    pub(super) fn wire(self) -> &'static str {
        match self {
            Self::FullControl => "full",
            Self::Proxy => "proxy",
        }
    }
}

struct Prompt {
    current: usize,
}

fn marker(index: usize, current: usize) -> String {
    let dot = if index == current { '*' } else { ' ' };
    format!("{}. ({dot})", index + 1)
}

impl Selector for Prompt {
    type Outcome = Mode;

    fn view(&self, _elapsed: Duration) -> View {
        View {
            title: String::new(),
            hints: Vec::new(),
            gap: false,
            rows: CHOICES
                .iter()
                .enumerate()
                .map(|(index, (_, label))| Row {
                    current: index == self.current,
                    marker: marker(index, self.current),
                    label: (*label).to_string(),
                    note: Note::None,
                })
                .collect(),
            footer: vec![PROMPT.into()],
        }
    }

    fn on_key(&mut self, key: Key) -> Step<Mode> {
        match key {
            Key::Up | Key::Down => {
                self.current = 1 - self.current;
                Step::Redraw
            }
            Key::Enter | Key::Space => Step::Done(CHOICES[self.current].0),
            Key::Char(digit) => parse_choice(&digit.to_string()).map_or(Step::Stay, Step::Done),
            Key::Escape => Step::Stay,
        }
    }
}

/// The typed form of a selection. Empty takes the marked default.
fn parse_choice(text: &str) -> Option<Mode> {
    match text.trim() {
        "" | "1" => Some(Mode::FullControl),
        "2" => Some(Mode::Proxy),
        _ => None,
    }
}

fn read_typed_choice(input: &mut impl BufRead, out: &mut impl Write) -> Result<Mode, String> {
    let prompt = Prompt { current: 0 };
    for (index, (_, label)) in CHOICES.iter().enumerate() {
        let _ = writeln!(out, "{} {label}", marker(index, prompt.current));
    }
    let _ = write!(out, "\n{PROMPT}");
    let _ = out.flush();
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => Err("no selection was made".into()),
        Ok(_) => parse_choice(&line).ok_or_else(|| "selection must be 1 or 2".into()),
    }
}

fn choose() -> Result<Mode, String> {
    let mut out = std::io::stdout();
    if std::io::stdin().is_terminal() && out.is_terminal() {
        return selector::run(&mut out, &mut Prompt { current: 0 })
            .map_err(|_| "no selection was made".to_string());
    }
    read_typed_choice(&mut std::io::stdin().lock(), &mut out)
}

pub fn run(host: &str, model: Option<&str>) -> i32 {
    if model.is_some() {
        eprintln!("{MODEL_RESERVED}");
        return 2;
    }
    let outcome = choose().and_then(|mode| match mode {
        Mode::FullControl => super::run_session(host, Some(mode)),
        Mode::Proxy => run_proxy_session(host),
    });
    outcome.unwrap_or_else(|error| {
        eprintln!("connect: {error}");
        1
    })
}

/// What one server event means while a proxy session runs.
#[derive(Debug, PartialEq, Eq)]
enum ProxyEvent {
    Approved,
    Ended(&'static str),
    Notice(&'static str),
    KeyRequest,
    Other,
}

fn classify(event: &Value) -> ProxyEvent {
    let kind = event["type"].as_str().unwrap_or("");
    let state = event["state"].as_str().unwrap_or("");
    match kind {
        "hello" | "state" if matches!(state, "accepted" | "connected") => ProxyEvent::Approved,
        "hello" | "state" if state == "expired" => {
            ProxyEvent::Ended("This keyword expired. Run `clud connect` again.")
        }
        "hello" | "state" => super::end_message(state).map_or(ProxyEvent::Other, ProxyEvent::Ended),
        "denied" => ProxyEvent::Ended("The operator denied this proxy request."),
        "disconnect" => super::end_message(kind).map_or(ProxyEvent::Other, ProxyEvent::Ended),
        "proxy_ended" => ProxyEvent::Notice("The operator ended the proxy."),
        "key_request" => ProxyEvent::KeyRequest,
        _ => ProxyEvent::Other,
    }
}

/// The agent runs with no credential but the session bearer. `--safe` is not
/// optional: every reply comes through the operator's server, so a tool call
/// must stop at the user's own permission prompt or this would be a shell.
fn start_agent(proxy: &proxy::SessionProxy, config: &Path) -> Result<ManagedSubprocess, String> {
    proxy::SessionProxy::seed_agent_config(config)
        .map_err(|_| "cannot prepare remote agent configuration")?;
    let exe = std::env::current_exe().map_err(|_| "cannot locate clud")?;
    ManagedSubprocess::start(
        agent_argv(exe.to_string_lossy().into_owned()),
        None,
        proxy.environment(std::env::vars().collect(), config),
        false,
        None,
    )
    .map_err(|error| format!("cannot start your agent: {error}"))
}

fn agent_argv(exe: String) -> Vec<String> {
    vec![exe, "--claude".into(), "--safe".into()]
}

struct ProxySession {
    socket: transport::Socket,
    proxy: proxy::SessionProxy,
    config: tempfile::TempDir,
    agent: Option<ManagedSubprocess>,
}

impl ProxySession {
    fn send(&mut self, event: Value) -> Result<(), String> {
        super::send(&mut self.socket, event)
    }

    /// Returns the exit code once the session is over before an agent started.
    fn handle(&mut self, event: &Value) -> Result<Option<i32>, String> {
        match classify(event) {
            ProxyEvent::Approved if self.agent.is_none() => {
                println!("{APPROVED}");
                self.agent = Some(start_agent(&self.proxy, self.config.path())?);
            }
            // The agent owns the terminal now; it reports refused requests itself.
            ProxyEvent::Notice(_) | ProxyEvent::Ended(_) if self.agent.is_some() => {}
            ProxyEvent::Notice(text) => println!("{text}"),
            ProxyEvent::Ended(text) => {
                println!("{text}");
                return Ok(Some(0));
            }
            ProxyEvent::KeyRequest => {
                let request_id = event["request_id"].clone();
                let reply = json!({"type":"key_supplied", "request_id":request_id, "stored":false});
                self.send(reply)?;
            }
            ProxyEvent::Approved | ProxyEvent::Other => {}
        }
        Ok(None)
    }

    /// One bounded socket read. `Some` is the exit code of a session that is over.
    fn pump(&mut self) -> Result<Option<i32>, String> {
        match self.socket.read() {
            Ok(Message::Text(text)) => {
                let event: Value =
                    serde_json::from_str(&text).map_err(|_| "invalid remote event")?;
                self.handle(&event)
            }
            Ok(Message::Close(_)) => Err("remote connection closed".into()),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(_) => Err("remote connection lost".into()),
            _ => Ok(None),
        }
    }
}

/// The agent is the user's own work: a lost server only refuses its requests.
fn wait_for_agent(agent: &ManagedSubprocess) -> i32 {
    agent.wait(None).unwrap_or(1)
}

fn run_proxy_session(host: &str) -> Result<i32, String> {
    let base = transport::base_url(host)?;
    let session = transport::create(&base, Some(Mode::Proxy.wire()))?;
    if session.mode.as_deref() != Some(Mode::Proxy.wire()) {
        return Err("this server cannot ask for proxy consent; nothing was enabled".into());
    }
    let proxy =
        proxy::SessionProxy::new(base.as_str(), &session.client_token, &session.proxy_model)?;
    let config = tempfile::Builder::new()
        .prefix("clud-remote-auth-")
        .tempdir()
        .map_err(|_| "cannot isolate remote agent credentials")?;
    let socket = match transport::connect(&session) {
        Ok(socket) => socket,
        Err(error) => {
            // The operator may have decided while the handshake was in flight.
            if let Some(message) = transport::terminal_status(&base, &session.keyword)
                .as_deref()
                .and_then(super::end_message)
            {
                println!("{message}");
                return Ok(0);
            }
            return Err(error);
        }
    };
    println!(
        "Asking the operator to approve {} through OpenRouter.\n\nTell them this keyword:\n{}",
        session.proxy_model, session.keyword
    );
    std::io::stdout()
        .flush()
        .map_err(|_| "cannot print remote keyword")?;
    let stop = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stop);
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))
        .map_err(|_| "cannot install remote cancellation handler")?;
    let mut state = ProxySession {
        socket,
        proxy,
        config,
        agent: None,
    };
    state.send(json!({"type":"caps", "caps":["proxy"], "client":concat!("clud/", env!("CARGO_PKG_VERSION"))}))?;
    drive(&mut state, &stop)
}

fn drive(state: &mut ProxySession, stop: &AtomicBool) -> Result<i32, String> {
    let mut heartbeat = Instant::now();
    loop {
        match &state.agent {
            Some(agent) => {
                if let Ok(Some(code)) = agent.poll() {
                    let _ = state.socket.close(None);
                    return Ok(code);
                }
            }
            // Once an agent runs, Ctrl+C is its keystroke, not a request to leave.
            None if stop.load(Ordering::Relaxed) => {
                let _ = state.socket.close(None);
                return Ok(130);
            }
            None => {}
        }
        if heartbeat.elapsed() >= Duration::from_secs(20) {
            heartbeat = Instant::now();
            if let Err(error) = state.send(json!({"type":"ping"})) {
                return finish_without_socket(state, error);
            }
        }
        match state.pump() {
            Ok(Some(code)) => return Ok(code),
            Ok(None) => {}
            Err(error) => return finish_without_socket(state, error),
        }
    }
}

fn finish_without_socket(state: &ProxySession, error: String) -> Result<i32, String> {
    match &state.agent {
        Some(agent) => Ok(wait_for_agent(agent)),
        None => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Args, Command};

    fn typed(input: &str) -> (Result<Mode, String>, String) {
        let mut out = Vec::new();
        let result = read_typed_choice(&mut input.as_bytes(), &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn the_prompt_is_the_requested_text_with_full_control_marked() {
        let (result, shown) = typed("\n");
        assert_eq!(
            shown,
            "1. (*) Allow Full Control\n2. ( ) Use Proxy\n\nMake your selection: "
        );
        assert_eq!(result, Ok(Mode::FullControl));
    }

    #[test]
    fn typed_selections_choose_and_anything_else_is_refused() {
        assert_eq!(typed("1\n").0, Ok(Mode::FullControl));
        assert_eq!(typed("2\n").0, Ok(Mode::Proxy));
        assert!(typed("3\n").0.is_err());
        assert!(typed("yes\n").0.is_err());
        // Closed stdin is not consent to the default.
        assert!(typed("").0.is_err());
    }

    #[test]
    fn the_selector_moves_the_mark_and_digits_choose_directly() {
        let mut prompt = Prompt { current: 0 };
        let rows = prompt.view(Duration::ZERO).rows;
        assert_eq!(rows[0].marker, "1. (*)");
        assert_eq!(rows[1].marker, "2. ( )");
        assert_eq!(prompt.on_key(Key::Down), Step::Redraw);
        assert_eq!(prompt.view(Duration::ZERO).rows[1].marker, "2. (*)");
        assert_eq!(prompt.on_key(Key::Enter), Step::Done(Mode::Proxy));
        assert_eq!(prompt.on_key(Key::Char('1')), Step::Done(Mode::FullControl));
        assert_eq!(prompt.on_key(Key::Char('9')), Step::Stay);
    }

    #[test]
    fn connect_parses_with_the_default_host_and_reserved_model() {
        let args = Args::try_parse_from(["clud", "connect"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Connect { ref host, model: None }) if host == "clud.bot"
        ));
        let args = Args::try_parse_from(["clud", "connect", "clud.bot", "--model", "x"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Connect { model: Some(ref m), .. }) if m == "x"
        ));
    }

    #[test]
    fn a_model_selection_is_refused_before_any_prompt_or_network() {
        assert_eq!(run("not a host", Some("deepseek")), 2);
    }

    #[test]
    fn only_the_operators_approval_starts_the_agent() {
        let of = |value: Value| classify(&value);
        assert_eq!(
            of(json!({"type":"hello", "state":"pending"})),
            ProxyEvent::Other
        );
        assert_eq!(
            of(json!({"type":"state", "state":"accepted"})),
            ProxyEvent::Approved
        );
        assert_eq!(
            of(json!({"type":"hello", "state":"connected"})),
            ProxyEvent::Approved
        );
        assert!(matches!(of(json!({"type":"denied"})), ProxyEvent::Ended(_)));
        assert!(matches!(
            of(json!({"type":"state", "state":"expired"})),
            ProxyEvent::Ended(_)
        ));
    }

    #[test]
    fn the_proxy_agent_always_keeps_its_permission_prompts() {
        let argv = agent_argv("clud".into());
        assert!(argv.iter().any(|arg| arg == "--safe"));
    }

    #[test]
    fn a_proxy_session_has_no_path_to_a_shell() {
        for kind in ["control_request", "control_granted", "pty_open", "exec"] {
            assert_eq!(classify(&json!({"type":kind})), ProxyEvent::Other, "{kind}");
        }
    }
}

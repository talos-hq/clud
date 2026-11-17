//! `clud remote connect` end to end on every host: the real binary and a real
//! shell against an in-process operator server, over real HTTP and WebSocket.
use crate::exe;
use running_process::pty::NativePtyProcess;
use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode,
};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

const WAIT: Duration = Duration::from_secs(60);
const TOKEN: &str = "ct_remote_connect_e2e_123456";
const KEYWORD: &str = "ABCDE-FGH23";

fn accept_before_deadline(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "client never connected");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    }
}

fn serve_session_creation(listener: &TcpListener) -> Value {
    let port = listener.local_addr().unwrap().port();
    let mut stream = accept_before_deadline(listener);
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    assert!(first.starts_with("POST /api/remote/sessions "), "{first}");
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
    }
    let mut request = vec![0; length];
    reader.read_exact(&mut request).unwrap();
    let body = json!({
        "keyword": KEYWORD,
        "client_token": TOKEN,
        "proxy_model": "m",
        "ws_url": format!("ws://127.0.0.1:{port}/ws/client?token={TOKEN}"),
    })
    .to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    serde_json::from_slice(&request).unwrap()
}

/// The foreground `clud remote connect`, with its output collected as it arrives.
struct Client {
    process: Arc<NativeProcess>,
    output: Arc<Mutex<String>>,
    _cwd: tempfile::TempDir,
}

impl Client {
    fn start(origin: &str) -> Self {
        let cwd = tempfile::tempdir().unwrap();
        let process = Arc::new(NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec![
                exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"))
                    .to_string_lossy()
                    .into_owned(),
                "remote".into(),
                "connect".into(),
                origin.into(),
            ]),
            cwd: Some(cwd.path().to_path_buf()),
            env: None,
            capture: true,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Piped,
            nice: None,
            address_space_limit_bytes: None,
        }));
        process.start().unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let (reader, sink) = (Arc::clone(&process), Arc::clone(&output));
        std::thread::spawn(move || loop {
            match reader.read_combined(Some(Duration::from_millis(50))) {
                ReadStatus::Line(event) => {
                    let mut text = sink.lock().unwrap();
                    text.push_str(&String::from_utf8_lossy(&event.line));
                    text.push('\n');
                }
                ReadStatus::Eof => break,
                ReadStatus::Timeout => {}
            }
        });
        Self {
            process,
            output,
            _cwd: cwd,
        }
    }

    fn text(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.text().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "client never showed {needle:?}: {}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn answer(&self, line: &str) {
        self.process
            .write_stdin_streaming(format!("{line}\n").as_bytes())
            .unwrap();
    }

    fn wait_exit(&self) -> i32 {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(code) = self.process.returncode() {
                return code;
            }
            assert!(Instant::now() < deadline, "client kept running");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

/// The operator's end of the client socket.
struct Operator {
    socket: WebSocket<TcpStream>,
    events: Vec<Value>,
    shell_output: Vec<u8>,
}

impl Operator {
    fn accept(listener: &TcpListener) -> Self {
        let stream = accept_before_deadline(listener);
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let socket = tungstenite::accept(stream).unwrap();
        socket
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        Self {
            socket,
            events: Vec::new(),
            shell_output: Vec::new(),
        }
    }

    fn send(&mut self, event: Value) {
        self.socket
            .send(Message::Text(event.to_string().into()))
            .unwrap();
    }

    fn type_into_shell(&mut self, command: &str) {
        let submit = if clud::platform::host::WINDOWS {
            "\r"
        } else {
            "\n"
        };
        let mut frame = vec![1];
        frame.extend_from_slice(format!("{command}{submit}").as_bytes());
        self.socket.send(Message::Binary(frame.into())).unwrap();
    }

    fn pump_once(&mut self) {
        match self.socket.read() {
            Ok(Message::Text(text)) => self.events.push(serde_json::from_str(&text).unwrap()),
            Ok(Message::Binary(frame)) => {
                assert_eq!(frame.first(), Some(&0), "output must use channel 0");
                self.shell_output.extend_from_slice(&frame[1..]);
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => panic!("client socket failed: {error}"),
        }
    }

    fn wait_event(&mut self, kind: &str) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(index) = self.events.iter().position(|event| event["type"] == kind) {
                return self.events.remove(index);
            }
            assert!(
                Instant::now() < deadline,
                "no {kind} event: {:?}",
                self.events
            );
            self.pump_once();
        }
    }

    fn wait_shell_output(&mut self, needle: &str) {
        let deadline = Instant::now() + WAIT;
        while !String::from_utf8_lossy(&self.shell_output).contains(needle) {
            assert!(
                Instant::now() < deadline,
                "server never received {needle:?}: {}",
                String::from_utf8_lossy(&self.shell_output)
            );
            self.pump_once();
        }
    }

    /// Drain briefly, for asserting that the client stays silent.
    fn settle(&mut self) {
        for _ in 0..20 {
            self.pump_once();
        }
    }
}

fn accepted_session() -> (Client, Operator, Value) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let client = Client::start(&origin);
    let created = serve_session_creation(&listener);
    let mut operator = Operator::accept(&listener);
    client.wait_for(KEYWORD);
    operator.send(json!({"type":"hello", "state":"pending"}));
    let caps = operator.wait_event("caps");
    assert_eq!(caps["caps"], json!(["pty"]));
    operator.send(json!({"type":"state", "state":"accepted"}));
    client.wait_for("Operator accepted the connection.");
    (client, operator, created)
}

/// What the tests need from a running client, piped or on a terminal.
trait ClientIo {
    fn text(&self) -> String;
    fn answer(&self, line: &str);

    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.text().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "client never showed {needle:?}: {}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl ClientIo for Client {
    fn text(&self) -> String {
        Client::text(self)
    }

    fn answer(&self, line: &str) {
        Client::answer(self, line);
    }
}

fn request_control(
    client: &impl ClientIo,
    operator: &mut Operator,
    id: &str,
    answer: &str,
) -> bool {
    let shown = client.text().matches("Allow Remote Control").count();
    operator.send(json!({"type":"control_request", "request_id":id}));
    let deadline = Instant::now() + WAIT;
    while client.text().matches("Allow Remote Control").count() == shown {
        assert!(Instant::now() < deadline, "no prompt: {}", client.text());
        std::thread::sleep(Duration::from_millis(10));
    }
    client.answer(answer);
    let response = operator.wait_event("control_response");
    assert_eq!(response["request_id"], id);
    response["allow"].as_bool().unwrap()
}

fn open_shell(client: &impl ClientIo, operator: &mut Operator) {
    assert!(request_control(client, operator, "grant", "y"));
    operator.send(json!({"type":"control_granted", "request_id":"grant"}));
    client.wait_for("Remote control granted");
    operator.send(json!({"type":"pty_open", "pty_id":"shell", "rows":24, "cols":80}));
    assert_eq!(operator.wait_event("pty_opened")["pty_id"], "shell");
}

/// A command whose output differs from its own echoed text.
fn print_marker() -> (&'static str, &'static str) {
    if clud::platform::host::WINDOWS {
        ("echo REMOTE_^MARKER", "REMOTE_MARKER")
    } else {
        ("printf 'REMOTE_%s\\n' MARKER", "REMOTE_MARKER")
    }
}

#[test]
fn session_reports_the_host_and_shows_the_keyword() {
    let (client, mut operator, created) = accepted_session();
    assert_eq!(created["client"]["os"], std::env::consts::OS);
    assert!(client.text().contains("connect#k=ABCDE-FGH23"));
    operator.send(json!({"type":"disconnect"}));
    client.wait_for("clud.bot has ended a connection");
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn nothing_reaches_a_shell_without_local_consent() {
    let (client, mut operator, _) = accepted_session();
    let refused = json!({"type":"pty_open", "pty_id":"early", "rows":24, "cols":80});
    operator.send(refused.clone());
    assert!(!request_control(&client, &mut operator, "first", "n"));
    // A grant for a refused request and input without one are both ignored.
    operator.send(json!({"type":"control_granted", "request_id":"first"}));
    operator.send(refused);
    operator
        .socket
        .send(Message::Binary(vec![1, b'x', b'\n'].into()))
        .unwrap();
    operator.settle();
    assert!(
        !operator.events.iter().any(|e| e["type"] == "pty_opened"),
        "{:?}",
        operator.events
    );
    assert!(operator.shell_output.is_empty());
    operator.send(json!({"type":"disconnect"}));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn shell_output_reaches_the_server_and_the_client_terminal() {
    let (client, mut operator, _) = accepted_session();
    open_shell(&client, &mut operator);
    let (command, marker) = print_marker();
    operator.type_into_shell(command);
    operator.wait_shell_output(marker);
    operator.send(json!({"type":"pty_resize", "pty_id":"shell", "rows":40, "cols":100}));
    operator.type_into_shell("exit 7");
    assert_eq!(operator.wait_event("pty_exit")["code"], 7);
    operator.send(json!({"type":"disconnect"}));
    client.wait_for("clud.bot has ended a connection");
    assert_eq!(client.wait_exit(), 0);
    // Only now: ConPTY positions text without newlines, and this harness reads lines.
    let shown = client.text();
    assert!(shown.contains(marker), "client terminal missed it: {shown}");
}

#[test]
fn revoked_control_closes_the_shell_and_requires_new_consent() {
    let (client, mut operator, _) = accepted_session();
    open_shell(&client, &mut operator);
    operator.send(json!({"type":"control_revoked"}));
    assert_eq!(operator.wait_event("pty_exit")["pty_id"], "shell");
    operator.send(json!({"type":"pty_open", "pty_id":"after", "rows":24, "cols":80}));
    operator.settle();
    assert!(!operator.events.iter().any(|e| e["type"] == "pty_opened"));
    assert!(!request_control(&client, &mut operator, "again", "n"));
    operator.send(json!({"type":"disconnect"}));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn operator_denial_ends_the_client_with_its_message() {
    let (client, mut operator, _) = accepted_session();
    operator.send(json!({"type":"denied"}));
    client.wait_for("clud.bot denied this connection.");
    assert_eq!(client.wait_exit(), 0);
}

/// `clud remote connect` on a terminal: what a user runs. Only then is the
/// password prompt hideable, so only then does the client offer `sudo`.
struct TtyClient {
    process: Arc<NativePtyProcess>,
    output: Arc<Mutex<String>>,
    _cwd: tempfile::TempDir,
}

impl TtyClient {
    fn start(origin: &str) -> Self {
        let cwd = tempfile::tempdir().unwrap();
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        env.push(("TERM".into(), "xterm-256color".into()));
        let argv = vec![
            exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"))
                .to_string_lossy()
                .into_owned(),
            "remote".into(),
            "connect".into(),
            origin.into(),
        ];
        let cwd_text = cwd.path().to_string_lossy().into_owned();
        let process = Arc::new(
            NativePtyProcess::new(argv, Some(cwd_text), Some(env), 40, 160, None).unwrap(),
        );
        process.start_impl().unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let (reader, sink) = (Arc::clone(&process), Arc::clone(&output));
        std::thread::spawn(move || loop {
            match reader.read_chunk_impl(Some(0.05)) {
                Ok(Some(bytes)) => sink
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&bytes)),
                Ok(None) => {}
                Err(_) => break,
            }
        });
        Self {
            process,
            output,
            _cwd: cwd,
        }
    }

    fn wait_exit(&self) -> Option<i32> {
        self.process.wait_impl(Some(WAIT.as_secs_f64())).ok()
    }
}

impl ClientIo for TtyClient {
    fn text(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    fn answer(&self, line: &str) {
        self.process
            .write_impl(format!("{line}\r").as_bytes(), false)
            .unwrap();
    }
}

impl Drop for TtyClient {
    fn drop(&mut self) {
        let _ = self.process.kill_tree_impl();
        let _ = self.process.close_impl();
    }
}

/// Run `command` in the operator's shell and return its exit status.
fn operator_status(operator: &mut Operator, command: &str, mark: &str) -> String {
    operator.type_into_shell(&format!("{command}; echo \"{mark}=$?\""));
    let deadline = Instant::now() + WAIT;
    loop {
        let shown = String::from_utf8_lossy(&operator.shell_output).into_owned();
        if let Some(at) = shown.rfind(&format!("{mark}=")) {
            let digits: String = shown[at + mark.len() + 1..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty() {
                return digits;
            }
        }
        assert!(Instant::now() < deadline, "no {mark}: {shown}");
        operator.pump_once();
    }
}

/// The operator asks for sudo; return the settled state that follows `prompting`.
fn request_sudo(client: &TtyClient, operator: &mut Operator, password: Option<&str>) -> Value {
    operator.send(json!({"type":"sudo_request", "request_id":"0123456789abcdef0123456789abcdef"}));
    let first = operator.wait_event("sudo_state");
    assert_eq!(first["state"], "prompting", "{first}");
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(index) = operator
            .events
            .iter()
            .position(|e| e["type"] == "sudo_state")
        {
            return operator.events.remove(index);
        }
        if client
            .text()
            .contains("Allow sudo for Zach Vorhies? [y/N]: ")
        {
            let Some(password) = password else {
                client.answer("n");
                return operator.wait_event("sudo_state");
            };
            client.answer("y");
            client.wait_for("(typing is hidden; it is never sent to clud.bot): ");
            client.answer(password);
            return operator.wait_event("sudo_state");
        }
        assert!(
            Instant::now() < deadline,
            "no sudo outcome: {}",
            client.text()
        );
        operator.pump_once();
    }
}

/// From a terminal the client offers `sudo` (not on Windows) and runs the
/// shell under `clud-remote-shell`; the shell's exit status still reaches
/// the operator. The sudo outcome depends on the host's sudo: a password
/// prompt is declined here, NOPASSWD reports `active`, no sudo `unavailable`.
/// With `CLUD_TEST_REAL_SUDO_PASSWORD` (a throwaway container user) the
/// password is typed, and `sudo -n true` in the operator's shell must
/// succeed after Request sudo and fail after Drop sudo.
#[test]
fn sudo_from_a_terminal_client_is_offered_consented_and_dropped() {
    if clud::platform::host::WINDOWS {
        return;
    }
    let password = std::env::var("CLUD_TEST_REAL_SUDO_PASSWORD").ok();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let client = TtyClient::start(&origin);
    serve_session_creation(&listener);
    let mut operator = Operator::accept(&listener);
    client.wait_for(KEYWORD);
    operator.send(json!({"type":"hello", "state":"pending"}));
    assert_eq!(operator.wait_event("caps")["caps"], json!(["pty", "sudo"]));
    operator.send(json!({"type":"state", "state":"accepted"}));
    client.wait_for("Operator accepted the connection.");
    open_shell(&client, &mut operator);
    let (command, marker) = print_marker();
    operator.type_into_shell(command);
    operator.wait_shell_output(marker);

    let outcome = request_sudo(&client, &mut operator, password.as_deref());
    let state = outcome["state"].as_str().unwrap().to_owned();
    assert!(
        ["active", "denied", "unavailable"].contains(&state.as_str()),
        "{outcome}"
    );
    if let Some(password) = &password {
        assert_eq!(state, "active", "{}", client.text());
        assert!(outcome["until"].as_u64().is_some(), "{outcome}");
        assert_eq!(
            operator_status(&mut operator, "sudo -n true", "AFTER_REQUEST"),
            "0"
        );
        operator.send(json!({"type":"sudo_drop"}));
        assert_eq!(operator.wait_event("sudo_state")["state"], "dropped");
        assert_eq!(
            operator_status(&mut operator, "sudo -n true", "AFTER_DROP"),
            "1"
        );
        assert!(
            !client.text().contains(password.as_str()),
            "{}",
            client.text()
        );
        let shell = String::from_utf8_lossy(&operator.shell_output).into_owned();
        assert!(!shell.contains(password.as_str()), "{shell}");
        for event in &operator.events {
            assert!(!event.to_string().contains(password.as_str()), "{event}");
        }
    }

    operator.type_into_shell("exit 7");
    assert_eq!(operator.wait_event("pty_exit")["code"], 7);
    operator.send(json!({"type":"disconnect"}));
    client.wait_for("clud.bot has ended a connection");
    assert_eq!(client.wait_exit(), Some(0));
}

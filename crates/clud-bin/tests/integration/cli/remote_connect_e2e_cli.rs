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
use std::path::{Path, PathBuf};
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

fn read_session_creation(listener: &TcpListener) -> (TcpStream, Value) {
    let stream = accept_before_deadline(listener);
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
    (stream, serde_json::from_slice(&request).unwrap())
}

fn serve_session_creation(listener: &TcpListener) -> Value {
    let port = listener.local_addr().unwrap().port();
    let (mut stream, request) = read_session_creation(listener);
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
    request
}

/// The foreground `clud remote connect`, with its output collected as it arrives.
struct Client {
    process: Arc<NativeProcess>,
    output: Arc<Mutex<String>>,
    _cwd: kernal_api::platform::fs::TemporaryDirectory,
}

impl Client {
    /// With `env` (added to this process's environment) set for the client.
    fn start_with(origin: &str, env: Option<(&str, &str)>) -> Self {
        let env = env.map(|(key, value)| {
            let mut vars: Vec<(String, String)> =
                std::env::vars().filter(|(name, _)| name != key).collect();
            vars.push((key.into(), value.into()));
            vars
        });
        let program = exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"));
        Self::launch(&program, origin, env)
    }

    /// `program` with exactly `env`, or this process's environment for `None`.
    fn launch(program: &Path, origin: &str, env: Option<Vec<(String, String)>>) -> Self {
        let cwd = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let process = Arc::new(NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec![
                program.to_string_lossy().into_owned(),
                "remote".into(),
                "connect".into(),
                origin.into(),
            ]),
            cwd: Some(cwd.path().to_path_buf()),
            env,
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
    /// Accept the client's socket and return the request path and query it used.
    #[expect(
        clippy::result_large_err,
        reason = "tungstenite's accept_hdr callback must return its ErrorResponse by value"
    )]
    fn accept_path(listener: &TcpListener) -> (Self, String) {
        let stream = accept_before_deadline(listener);
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let mut path = String::new();
        let socket = tungstenite::accept_hdr(
            stream,
            |request: &tungstenite::handshake::server::Request,
             response: tungstenite::handshake::server::Response| {
                path = request.uri().to_string();
                Ok(response)
            },
        )
        .unwrap();
        // A timed-out blocking recv can leave a Windows socket unusable.
        socket.get_ref().set_read_timeout(None).unwrap();
        socket.get_ref().set_nonblocking(true).unwrap();
        (
            Self {
                socket,
                events: Vec::new(),
                shell_output: Vec::new(),
            },
            path,
        )
    }

    fn send(&mut self, event: Value) {
        self.send_frame(Message::Text(event.to_string().into()));
    }

    fn send_frame(&mut self, frame: Message) {
        match self.socket.send(frame) {
            Ok(()) => return,
            Err(tungstenite::Error::Io(error)) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => panic!("cannot send operator frame: {error}"),
        }
        // The frame was already queued. Retry its flush without sending it twice.
        let deadline = Instant::now() + WAIT;
        loop {
            match self.socket.flush() {
                Ok(()) => return,
                Err(tungstenite::Error::Io(error)) if error.kind() == ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "operator socket did not drain");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("cannot flush operator frame: {error}"),
            }
        }
    }

    fn type_into_shell(&mut self, command: &str) {
        let submit = if clud::platform::host::WINDOWS {
            "\r"
        } else {
            "\n"
        };
        let mut frame = vec![1];
        frame.extend_from_slice(format!("{command}{submit}").as_bytes());
        self.send_frame(Message::Binary(frame.into()));
    }

    /// Read one frame. A heartbeat `ping` is answered like the real server
    /// does: the client counts 35 s without a frame as a lost link, and a long
    /// flood (Windows relays about 200 KB/s through ConPTY) outlasts that
    /// while the operator only reads (#2887).
    fn pump_once(&mut self) {
        match self.socket.read() {
            Ok(Message::Text(text)) => {
                let event: Value = serde_json::from_str(&text).unwrap();
                if event["type"] == "ping" {
                    self.send(json!({"type":"pong"}));
                } else {
                    self.events.push(event);
                }
            }
            Ok(Message::Binary(frame)) => {
                assert_eq!(frame.first(), Some(&0), "output must use channel 0");
                self.shell_output.extend_from_slice(&frame[1..]);
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error)) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
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

    /// Like `wait_shell_output`, but searches only what arrived since the last
    /// look, for floods where rescanning everything would be quadratic.
    ///
    /// The deadline is `WAIT` without progress, not `WAIT` in total: a flood
    /// moves at the runner's throughput (Windows runners relay about 200 KB/s
    /// through ConPTY, #2887), and what this guards against is a link that
    /// stops delivering, not a slow one.
    fn wait_shell_bytes(&mut self, needle: &[u8], client: &Client, listener: &TcpListener) {
        let mut deadline = Instant::now() + WAIT;
        let mut seen = self.shell_output.len();
        let mut from = 0;
        loop {
            let window = &self.shell_output[from.min(self.shell_output.len())..];
            if window.windows(needle.len()).any(|w| w == needle) {
                return;
            }
            from = self.shell_output.len().saturating_sub(needle.len());
            if self.shell_output.len() > seen {
                seen = self.shell_output.len();
                deadline = Instant::now() + WAIT;
            }
            if Instant::now() >= deadline {
                let client_text = client.text();
                let client_tail = &client_text.as_bytes()[client_text.len().saturating_sub(600)..];
                panic!(
                    "server never received {:?}: nothing new for {WAIT:?} ({} bytes so far), \
                     last bytes: {}; client exit: {:?}, reconnect waiting: {}, client tail: {}",
                    String::from_utf8_lossy(needle),
                    self.shell_output.len(),
                    self.shell_output[self.shell_output.len().saturating_sub(300)..].escape_ascii(),
                    client.process.returncode(),
                    listener.accept().is_ok(),
                    client_tail.escape_ascii(),
                );
            }
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
    let (client, operator, created, _) =
        session_with(json!({"type":"hello", "state":"pending"}), None);
    (client, operator, created)
}

/// A session whose server supports resume; the listener takes the reconnects.
fn resumable_session(env: Option<(&str, &str)>) -> (Client, Operator, TcpListener) {
    let hello = json!({"type":"hello", "state":"pending", "resume":{"control":"none"}});
    let (client, operator, _, listener) = session_with(hello, env);
    (client, operator, listener)
}

fn session_with(hello: Value, env: Option<(&str, &str)>) -> (Client, Operator, Value, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let client = Client::start_with(&origin, env);
    let created = serve_session_creation(&listener);
    let (mut operator, path) = Operator::accept_path(&listener);
    assert!(path.contains("&resume=1"), "client declares resume: {path}");
    assert!(
        path.ends_with("&e2e=1"),
        "client asks for encryption: {path}"
    );
    client.wait_for(KEYWORD);
    operator.send(hello);
    let caps = operator.wait_event("caps");
    assert_eq!(caps["caps"], json!(["pty", "resume"]));
    operator.send(json!({"type":"state", "state":"accepted"}));
    client.wait_for("Operator accepted the connection.");
    (client, operator, created, listener)
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

/// What connect.ps1 and connect.sh do: a clud copied into a private directory
/// on no PATH, `%LOCALAPPDATA%\clud-connect-<guid>\bin` on Windows and
/// `.clud-connect.XXXXXX/bin` elsewhere, run from there. Returns the
/// directory's owner and the copy.
fn private_clud_copy() -> (kernal_api::platform::fs::TemporaryDirectory, PathBuf) {
    let source = exe::bin_path("clud", option_env!("CARGO_BIN_EXE_clud"));
    let (parent, prefix) = match std::env::var_os("LOCALAPPDATA") {
        Some(local) if clud::platform::host::WINDOWS => (PathBuf::from(local), "clud-connect-"),
        _ => (std::env::temp_dir(), ".clud-connect."),
    };
    let work = kernal_api::platform::fs::TemporaryDirectory::in_directory(&parent, prefix).unwrap();
    let bin = work.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let copy = bin.join(source.file_name().unwrap());
    std::fs::copy(&source, &copy).unwrap();
    (work, copy)
}

/// This process's environment with PATH cut down to system directories, none
/// holding a clud, so only the session's own binary can answer `clud`.
fn system_only_environment(clud_file_name: &std::ffi::OsStr) -> Vec<(String, String)> {
    let dirs: Vec<PathBuf> = if clud::platform::host::WINDOWS {
        let root =
            PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()));
        vec![
            root.join("System32"),
            root.join("System32").join("Wbem"),
            root,
        ]
    } else {
        vec!["/usr/bin".into(), "/bin".into()]
    };
    for dir in &dirs {
        let other = dir.join(clud_file_name);
        assert!(!other.exists(), "{} would hide the bug", other.display());
    }
    let path = std::env::join_paths(&dirs).unwrap();
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| !key.eq_ignore_ascii_case("PATH"))
        .collect();
    env.push(("PATH".into(), path.to_string_lossy().into_owned()));
    env
}

/// `clud` typed into the operator's shell is the clud serving the session,
/// even when that clud was never installed: the one-liners run a private copy.
#[test]
fn the_operator_shell_runs_the_session_clud_from_a_private_copy() {
    let (_work, copy) = private_clud_copy();
    let env = system_only_environment(copy.file_name().unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let client = Client::launch(&copy, &origin, Some(env));
    serve_session_creation(&listener);
    let (mut operator, _) = Operator::accept_path(&listener);
    client.wait_for(KEYWORD);
    operator.send(json!({"type":"hello", "state":"pending"}));
    operator.wait_event("caps");
    operator.send(json!({"type":"state", "state":"accepted"}));
    client.wait_for("Operator accepted the connection.");
    open_shell(&client, &mut operator);
    operator.type_into_shell(if clud::platform::host::WINDOWS {
        "clud --version & echo AFTER_^VERSION"
    } else {
        "clud --version; printf 'AFTER_%s\\n' VERSION"
    });
    operator.wait_shell_output("AFTER_VERSION");
    let shown = String::from_utf8_lossy(&operator.shell_output).into_owned();
    let version = format!("clud {}", env!("CARGO_PKG_VERSION"));
    assert!(shown.contains(&version), "shell did not run clud: {shown}");
    operator.send(json!({"type":"disconnect"}));
    assert_eq!(client.wait_exit(), 0);
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

/// "Open Address" needs granted control, and even then a `file:` address is
/// refused before any browser is launched. No accepted address is sent here,
/// so the test never opens a browser.
#[test]
fn open_address_is_refused_without_control_and_for_a_local_file() {
    let (client, mut operator, _) = accepted_session();
    operator.send(json!({"type":"open_url", "url":"https://clud.bot/connect"}));
    client.wait_for("remote control is not granted");
    assert!(request_control(&client, &mut operator, "grant", "y"));
    operator.send(json!({"type":"control_granted", "request_id":"grant"}));
    client.wait_for("Remote control granted");
    operator.send(json!({"type":"open_url", "url":"file:///etc/passwd"}));
    client.wait_for("refused to open an address (only https addresses are allowed)");
    assert!(!client.text().contains("opened in your browser"));
    operator.send(json!({"type":"disconnect"}));
    assert_eq!(client.wait_exit(), 0);
}

/// `clud remote connect` on a terminal: what a user runs. Only then is the
/// password prompt hideable, so only then does the client offer `sudo`.
struct TtyClient {
    process: Arc<NativePtyProcess>,
    output: Arc<Mutex<String>>,
    _cwd: kernal_api::platform::fs::TemporaryDirectory,
}

impl TtyClient {
    fn start(origin: &str) -> Self {
        let cwd = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
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
    let (mut operator, _) = Operator::accept_path(&listener);
    client.wait_for(KEYWORD);
    operator.send(json!({"type":"hello", "state":"pending"}));
    assert_eq!(
        operator.wait_event("caps")["caps"],
        json!(["pty", "resume", "sudo"])
    );
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

const LOST: &str = "Connection to clud.bot lost; reconnecting";

/// The connection dies without a close frame (a crashed server, a dead link).
fn drop_connection(operator: Operator) {
    drop(operator);
}

/// Accept the client's reconnect and answer it with the server's record.
fn resume_with(listener: &TcpListener, record: Value) -> Operator {
    let (mut operator, path) = Operator::accept_path(listener);
    assert!(path.contains(&format!("token={TOKEN}")), "{path}");
    assert!(path.ends_with("&resume=1&e2e=1"), "{path}");
    operator.send(json!({"type":"hello", "state":"connected", "resume":record}));
    operator.wait_event("caps");
    operator
}

/// Set a variable in the shell, then print it so the output differs from the command.
fn remember_and_recall() -> (&'static str, &'static str, &'static str) {
    if clud::platform::host::WINDOWS {
        ("set MARK=ESUMED_SHELL", "echo R%MARK%", "RESUMED_SHELL")
    } else {
        (
            "MARK=ESUMED_SHELL",
            "printf 'R%s\\n' \"$MARK\"",
            "RESUMED_SHELL",
        )
    }
}

/// Output the shell writes a moment later, while the link is down.
fn print_later() -> (&'static str, &'static str) {
    if clud::platform::host::WINDOWS {
        (
            "ping -n 4 127.0.0.1 >nul & echo LATE_^OUTPUT",
            "LATE_OUTPUT",
        )
    } else {
        ("sleep 2; printf 'LATE_%s\\n' OUTPUT", "LATE_OUTPUT")
    }
}

#[test]
fn a_lost_connection_resumes_the_same_shell_with_output_from_the_outage() {
    let (client, mut operator, listener) = resumable_session(None);
    open_shell(&client, &mut operator);
    let (remember, recall, recalled) = remember_and_recall();
    operator.type_into_shell(remember);
    let (later, late) = print_later();
    operator.type_into_shell(later);
    // Cut only once the client has read the input: a reset discards data the
    // client's kernel received but the client had not read yet. The echo of
    // the second command means both reached the shell, in order.
    operator.wait_shell_output("LATE_");
    drop_connection(operator);
    client.wait_for(LOST);
    // Let the shell write while nobody is connected.
    std::thread::sleep(Duration::from_secs(4));
    let mut operator = resume_with(
        &listener,
        json!({"control":"granted", "grant_id":"grant", "control_request":null}),
    );
    let report = operator.wait_event("resume");
    assert_eq!(report["pty"]["pty_id"], "shell", "{report}");
    client.wait_for("Reconnected.");
    operator.wait_shell_output(late);
    operator.type_into_shell(recall);
    operator.wait_shell_output(recalled);
    assert_eq!(client.text().matches(LOST).count(), 1, "{}", client.text());
    operator.send(json!({"type":"disconnect"}));
    client.wait_for("clud.bot has ended a connection");
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn a_silent_socket_is_replaced_and_the_same_shell_resumes() {
    let (client, mut silent, listener) = resumable_session(None);
    open_shell(&client, &mut silent);
    let (remember, recall, recalled) = remember_and_recall();
    silent.type_into_shell(remember);
    silent.wait_shell_output(remember);

    // Keep the TCP stream open and stop reading or answering heartbeats.
    // The client must detect loss without a close frame, FIN, or RST.
    client.wait_for(LOST);
    let mut operator = resume_with(&listener, json!({"control":"granted","grant_id":"grant"}));
    assert_eq!(operator.wait_event("resume")["pty"]["pty_id"], "shell");
    drop(silent);

    operator.type_into_shell(recall);
    operator.wait_shell_output(recalled);
    operator.send(json!({"type":"disconnect"}));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn control_revoked_while_away_is_not_regained_on_resume() {
    let (client, mut operator, listener) = resumable_session(None);
    open_shell(&client, &mut operator);
    drop_connection(operator);
    client.wait_for(LOST);
    let mut operator = resume_with(&listener, json!({"control":"none", "grant_id":null}));
    let report = operator.wait_event("resume");
    assert!(
        report["pty"].is_null(),
        "no terminal without the grant: {report}"
    );
    client.wait_for("Remote control ended while the connection was down.");
    operator.send(json!({"type":"pty_open", "pty_id":"again", "rows":24, "cols":80}));
    operator
        .socket
        .send(Message::Binary(vec![1, b'x', b'\n'].into()))
        .unwrap();
    operator.settle();
    assert!(!operator.events.iter().any(|e| e["type"] == "pty_opened"));
    assert!(operator.shell_output.is_empty());
    operator.send(json!({"type":"disconnect"}));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn a_grant_the_client_does_not_hold_is_released_not_used() {
    let (client, operator, listener) = resumable_session(None);
    drop_connection(operator);
    client.wait_for(LOST);
    let mut operator = resume_with(&listener, json!({"control":"granted", "grant_id":"forged"}));
    operator.wait_event("control_release");
    assert!(operator.wait_event("resume")["pty"].is_null());
    operator.send(json!({"type":"pty_open", "pty_id":"forged", "rows":24, "cols":80}));
    operator.settle();
    assert!(!operator.events.iter().any(|e| e["type"] == "pty_opened"));
    operator.send(json!({"type":"disconnect"}));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn a_question_pending_across_a_resume_is_asked_once_and_an_offline_answer_is_sent() {
    let (client, mut operator, listener) = resumable_session(None);
    operator.send(json!({"type":"control_request", "request_id":"q1"}));
    client.wait_for("Allow Remote Control");
    drop_connection(operator);
    client.wait_for(LOST);
    // The user answers while the connection is down.
    client.answer("y");
    std::thread::sleep(Duration::from_millis(300));
    let mut operator = resume_with(
        &listener,
        json!({"control":"requested", "control_request":"q1", "grant_id":null}),
    );
    let response = operator.wait_event("control_response");
    assert_eq!(
        (response["request_id"].as_str(), response["allow"].as_bool()),
        (Some("q1"), Some(true))
    );
    operator.send(json!({"type":"control_granted", "request_id":"q1"}));
    client.wait_for("Remote control granted");
    assert_eq!(
        client.text().matches("Allow Remote Control").count(),
        1,
        "{}",
        client.text()
    );
    operator.send(json!({"type":"disconnect"}));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn an_unanswered_question_survives_a_resume_without_a_second_prompt() {
    let (client, mut operator, listener) = resumable_session(None);
    operator.send(json!({"type":"control_request", "request_id":"q2"}));
    client.wait_for("Allow Remote Control");
    drop_connection(operator);
    client.wait_for(LOST);
    let mut operator = resume_with(
        &listener,
        json!({"control":"requested", "control_request":"q2"}),
    );
    operator.wait_event("resume");
    client.wait_for("Reconnected.");
    client.answer("n");
    let response = operator.wait_event("control_response");
    assert_eq!(response["allow"], false);
    assert_eq!(client.text().matches("Allow Remote Control").count(), 1);
    operator.send(json!({"type":"disconnect"}));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn a_restart_notice_reconnects_and_two_outages_in_a_row_resume() {
    let (client, mut operator, listener) = resumable_session(None);
    open_shell(&client, &mut operator);
    operator.send(json!({"type":"server_restart", "retry_after_ms":1000}));
    let _ = operator.socket.close(None);
    let _ = operator.socket.flush();
    client.wait_for(LOST);
    let granted = json!({"control":"granted", "grant_id":"grant"});
    let operator = resume_with(&listener, granted.clone());
    client.wait_for("Reconnected.");
    // A second restart right after the first.
    drop_connection(operator);
    let mut operator = resume_with(&listener, granted);
    assert_eq!(operator.wait_event("resume")["pty"]["pty_id"], "shell");
    let (command, marker) = print_marker();
    operator.type_into_shell(command);
    operator.wait_shell_output(marker);
    assert_eq!(
        client.text().matches("Reconnected.").count(),
        2,
        "{}",
        client.text()
    );
    operator.send(json!({"type":"disconnect"}));
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn a_newer_connection_with_the_same_token_ends_this_one() {
    let (client, mut operator, _listener) = resumable_session(None);
    operator
        .socket
        .close(Some(tungstenite::protocol::CloseFrame {
            code: tungstenite::protocol::frame::coding::CloseCode::from(4409),
            reason: "".into(),
        }))
        .unwrap();
    let _ = operator.socket.flush();
    client.wait_for("This session was resumed from another connection.");
    assert_ne!(client.wait_exit(), 0);
    assert!(!client.text().contains(LOST));
}

#[test]
fn the_client_gives_up_after_its_window_with_a_failure_code() {
    let (client, operator, listener) =
        resumable_session(Some(("CLUD_REMOTE_RESUME_WINDOW_SECS", "3")));
    drop(listener);
    drop_connection(operator);
    client.wait_for(LOST);
    let code = client.wait_exit();
    assert_eq!(code, 1, "{}", client.text());
    client.wait_for("could not reconnect to clud.bot in time");
}

#[test]
fn a_server_without_resume_ends_the_client_as_before() {
    let (client, operator, _) = accepted_session();
    drop_connection(operator);
    assert_eq!(client.wait_exit(), 1);
    client.wait_for("remote connection lost");
    assert!(!client.text().contains(LOST), "{}", client.text());
}

/// This mock is an old server: it ignores `e2e=1` and greets in plaintext.
#[test]
fn a_server_without_encryption_is_used_with_a_warning() {
    let (client, mut operator, created) = accepted_session();
    // Not expected to encrypt (a loopback host), so the description is sent as before.
    assert!(created["client"].get("cwd").is_some(), "{created}");
    // The operator tells machines apart by name (#2899).
    let host = created["client"]["host"].as_str().unwrap_or_default();
    assert!(!host.is_empty(), "{created}");
    assert!(created["client"].get("user").is_some(), "{created}");
    client.wait_for("does not offer end-to-end encryption");
    operator.send(json!({"type":"disconnect"}));
    client.wait_for("clud.bot has ended a connection");
    assert_eq!(client.wait_exit(), 0);
}

#[test]
fn a_server_without_encryption_is_refused_when_encryption_is_required() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let client = Client::start_with(&origin, Some(("CLUD_REMOTE_REQUIRE_E2E", "1")));
    let created = serve_session_creation(&listener);
    // Kept for the encrypted socket, never sent where TLS ends at the edge.
    assert!(created["client"].get("cwd").is_none(), "{created}");
    assert!(created["client"].get("host").is_none(), "{created}");
    assert!(created["client"].get("user").is_none(), "{created}");
    let (mut operator, path) = Operator::accept_path(&listener);
    assert!(
        path.contains("&e2e=1"),
        "client asks for encryption: {path}"
    );
    operator.send(json!({"type":"hello", "state":"pending"}));
    client.wait_for("did not offer end-to-end encryption; refusing");
    assert_eq!(client.wait_exit(), 1);
    assert!(!client.text().contains("Keyword:"), "{}", client.text());
}

/// Numbered lines in a file, and the shell command that prints it.
fn numbered_file(lines: usize) -> (kernal_api::platform::fs::TemporaryDirectory, String) {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = dir.path().join("numbers.txt");
    let mut text = String::with_capacity(lines * 9);
    for number in 1..=lines {
        text.push_str(&number.to_string());
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    let shown = path.display();
    let command = if clud::platform::host::WINDOWS {
        format!("type \"{shown}\"")
    } else {
        format!("cat '{shown}'")
    };
    (dir, command)
}

/// The numbered lines the operator received, in arrival order.
fn numbers_received(output: &[u8]) -> Vec<usize> {
    String::from_utf8_lossy(output)
        .split(['\r', '\n'])
        .filter(|line| !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()))
        .map(|line| line.parse().unwrap())
        .collect()
}

/// A congested link: the server stops reading while the shell floods the
/// session, long past any write timeout. The client must wait the stall out on
/// the same socket, pacing the shell, and deliver every line in order: a
/// reconnect would lose what the old socket had in flight.
#[test]
fn a_stalled_link_paces_the_shell_and_loses_nothing() {
    const LINES: usize = 2_500_000;
    let (client, mut operator, listener) = resumable_session(None);
    open_shell(&client, &mut operator);
    let (_dir, print) = numbered_file(LINES);
    operator.type_into_shell(&print);
    operator.wait_shell_bytes(b"\n1000\r\n", &client, &listener);
    // Stop reading: the server's receive buffer and the client's send buffer fill.
    std::thread::sleep(Duration::from_secs(12));
    // The bare number, not a framed line: ConPTY renders the screen, and after
    // a stall it may repaint the last lines with escapes around them (#2887).
    // No smaller line contains it, so it still marks the end of the flood.
    operator.wait_shell_bytes(LINES.to_string().as_bytes(), &client, &listener);
    listener.set_nonblocking(true).unwrap();
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == ErrorKind::WouldBlock),
        "the client reconnected instead of waiting out the stall"
    );
    assert!(!client.text().contains(LOST), "{}", client.text());
    let numbers = numbers_received(&operator.shell_output);
    if !clud::platform::host::WINDOWS {
        // ConPTY renders the screen and may coalesce a flood; POSIX relays every byte.
        assert_eq!(numbers.len(), LINES, "lines were lost or repeated");
        assert!(numbers.iter().enumerate().all(|(i, n)| *n == i + 1));
    }
    operator.send(json!({"type":"disconnect"}));
    client.wait_for("clud.bot has ended a connection");
    assert_eq!(client.wait_exit(), 0);
}

/// A poor link drops the first try at each startup step: creating the session
/// and opening its socket. Each is retried, and the session starts as usual.
#[test]
fn a_dropped_first_attempt_at_startup_is_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let client = Client::start_with(&origin, None);
    // The relay receives a complete create request, then its response is lost.
    // The unclaimed server-side session expires. A retry must reach consent
    // using only the token from the second HTTP response.
    let (first, body) = read_session_creation(&listener);
    drop(first);
    assert_eq!(serve_session_creation(&listener), body);
    // The socket's first handshake is lost too.
    drop(accept_before_deadline(&listener));
    let (mut operator, path) = Operator::accept_path(&listener);
    assert!(path.contains(&format!("token={TOKEN}")), "{path}");
    client.wait_for(KEYWORD);
    client.wait_for("retrying");
    operator.send(json!({"type":"hello", "state":"pending"}));
    operator.wait_event("caps");
    operator.send(json!({"type":"disconnect"}));
    client.wait_for("clud.bot has ended a connection");
    assert_eq!(client.wait_exit(), 0);
}

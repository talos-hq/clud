//! Bounded terminal pumps over the existing running-process PTY boundary.
use super::paste::{InputFilter, OutputTracker, PasteMode};
use crate::platform::remote_shell::{LeaderListener, LeaderStream};
use running_process::pty::NativePtyProcess;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::time::Duration;
use tungstenite::Message;

const FRAME_BYTES: usize = 16 * 1024;
/// Operator input allowed to wait for the shell. A TUI with mouse tracking on
/// gets a report per pointer move, in bursts, while it may be slow to read; a
/// frame count overflowed and ended the session. The server already limits
/// input to ~32 KiB/s, so a full budget means the shell has stopped reading:
/// later frames are dropped and the operator sees a notice, the session stays.
const INPUT_BUDGET_BYTES: usize = 1024 * 1024;
const OUTPUT_FRAMES: usize = 128;
/// ConPTY's first output: a cursor query it holds the shell behind until answered.
const CONPTY_STARTUP_QUERY: &[u8] = b"\x1b[6n";
const CONPTY_STARTUP_REPLY: &[u8] = b"\x1b[1;1R";
/// The leader connects before it starts the shell; this bounds a failure.
const LEADER_CONNECT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Default)]
pub(super) struct Terminal {
    process: Option<Arc<NativePtyProcess>>,
    id: Option<String>,
    input: Option<InputQueue>,
    output: Option<mpsc::Receiver<Message>>,
    failed: Arc<AtomicBool>,
    command: String,
    environment: Option<Vec<(String, String)>>,
    config: Option<tempfile::TempDir>,
    /// Start the shell under `clud-remote-shell` so sudo can be shared.
    leader_enabled: bool,
    /// The connected leader, until the sudo flow takes it.
    leader: Option<LeaderStream>,
    /// `(rows, cols)` of the open terminal, reported when a session resumes.
    size: (u16, u16),
}

fn dimensions(event: &Value) -> (u16, u16) {
    let cols = event["cols"].as_u64().unwrap_or(80).clamp(20, 500) as u16;
    let rows = event["rows"].as_u64().unwrap_or(24).clamp(5, 200) as u16;
    (rows, cols)
}

fn stop_process(process: &NativePtyProcess) {
    let _ = process.kill_tree_impl();
    let _ = process.close_impl();
}

/// What [`Terminal::input`] did with a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Delivery {
    Queued,
    /// Over [`INPUT_BUDGET_BYTES`] waiting: dropped, the session continues.
    Dropped,
}

/// Operator input waiting for the shell, bounded by bytes rather than frames.
struct InputQueue {
    tx: mpsc::Sender<Vec<u8>>,
    pending: Arc<AtomicUsize>,
}

impl InputQueue {
    fn push(&self, data: &[u8]) -> Delivery {
        if self.pending.load(Ordering::Relaxed) + data.len() > INPUT_BUDGET_BYTES {
            return Delivery::Dropped;
        }
        self.pending.fetch_add(data.len(), Ordering::Relaxed);
        // A closed queue means the pump ended; the output side reports why.
        let _ = self.tx.send(data.to_vec());
        Delivery::Queued
    }
}

fn input_pump(
    process: Arc<NativePtyProcess>,
    rx: mpsc::Receiver<Vec<u8>>,
    pending: Arc<AtomicUsize>,
    failed: Arc<AtomicBool>,
    mut paste: InputFilter,
) {
    for bytes in rx {
        pending.fetch_sub(bytes.len(), Ordering::Relaxed);
        let bytes = paste.filter(&bytes);
        if bytes.is_empty() {
            continue;
        }
        if process.write_impl(&bytes, false).is_err() {
            if !process.wait_for_reader_closed_impl(Some(0.0)) {
                failed.store(true, Ordering::Relaxed);
            }
            break;
        }
    }
}

/// `chunk` without ConPTY's startup query, if it holds one. The operator's
/// terminal cannot be relied on to answer: it may not be attached, and only the
/// socket holding control may send input. So the client alone answers this one,
/// and hides it, since a second reply would reach the shell as typed junk.
fn without_startup_query(chunk: &[u8]) -> Option<Vec<u8>> {
    let at = chunk
        .windows(CONPTY_STARTUP_QUERY.len())
        .position(|window| window == CONPTY_STARTUP_QUERY)?;
    let mut rest = chunk[..at].to_vec();
    rest.extend_from_slice(&chunk[at + CONPTY_STARTUP_QUERY.len()..]);
    Some(rest)
}

/// Frames the session loop may forward per pass: the whole output queue, so a
/// pass empties it rather than letting the socket's read timeout pace output.
pub(super) const OUTPUT_DRAIN: usize = OUTPUT_FRAMES;
/// How often a pump held back by a full output queue rechecks it.
const BACKPRESSURE_RECHECK: Duration = Duration::from_millis(2);

/// Coalesce output already waiting into `bytes`, up to one frame. A burst of
/// small writes (a progress bar) otherwise costs a frame per write.
fn gather(process: &NativePtyProcess, bytes: &mut Vec<u8>) {
    while bytes.len() < FRAME_BYTES {
        match process.read_chunk_impl(Some(0.0)) {
            Ok(Some(more)) => bytes.extend_from_slice(&more),
            _ => break,
        }
    }
}

/// Queue `message` for the socket, waiting while the queue is full. The wait
/// holds the PTY reader's lock, so running-process's reader stops draining the
/// PTY: the kernel buffer fills and the shell's writes block. A burst is slowed
/// to the socket's pace instead of ending the session, and memory stays at the
/// queue plus one read. `false` once the terminal is stopped: the receiver is
/// gone, or `close_impl` has taken the handles and needs this lock to finish.
fn deliver(process: &NativePtyProcess, tx: &mpsc::SyncSender<Message>, message: Message) -> bool {
    let mut message = match tx.try_send(message) {
        Ok(()) => return true,
        Err(mpsc::TrySendError::Disconnected(_)) => return false,
        Err(mpsc::TrySendError::Full(message)) => message,
    };
    let _reader = process.reader.state.lock();
    loop {
        let closing = process
            .handles
            .lock()
            .ok()
            .is_none_or(|handles| handles.is_none());
        if closing {
            return false;
        }
        std::thread::sleep(BACKPRESSURE_RECHECK);
        message = match tx.try_send(message) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(message)) => message,
        };
    }
}

fn exit_event(id: &str, code: Option<i32>) -> Message {
    let event = json!({"type":"pty_exit", "pty_id":id, "code":code, "signal":null});
    Message::Text(event.to_string().into())
}

/// Forward one read; `false` once the terminal is stopped.
fn forward(
    process: &NativePtyProcess,
    tx: &mpsc::SyncSender<Message>,
    paste: &mut OutputTracker,
    mut bytes: Vec<u8>,
) -> bool {
    gather(process, &mut bytes);
    paste.observe(&bytes);
    bytes.chunks(FRAME_BYTES).all(|chunk| {
        let mut frame = vec![0];
        frame.extend_from_slice(chunk);
        deliver(process, tx, Message::Binary(frame.into()))
    })
}

fn output_pump(
    process: Arc<NativePtyProcess>,
    tx: mpsc::SyncSender<Message>,
    id: String,
    failed: Arc<AtomicBool>,
    mut paste: OutputTracker,
) {
    let mut awaiting_startup_query = crate::platform::host::WINDOWS;
    let failure = loop {
        match process.read_chunk_impl(Some(0.1)) {
            Ok(Some(mut bytes)) => {
                if std::mem::take(&mut awaiting_startup_query) {
                    if let Some(rest) = without_startup_query(&bytes) {
                        bytes = rest;
                        let _ = process.write_impl(CONPTY_STARTUP_REPLY, false);
                    }
                }
                if !forward(&process, &tx, &mut paste, bytes) {
                    break false;
                }
            }
            // ConPTY keeps its output open after the child exits, so a reader
            // never reaches EOF there. Idle means the output is already forwarded.
            Ok(None) => {
                let exited =
                    running_process::pty::poll_pty_process(&process.handles, &process.returncode);
                if let Ok(Some(code)) = exited {
                    if deliver(&process, &tx, exit_event(&id, Some(code))) {
                        return;
                    }
                    break false;
                }
            }
            Err(_) if process.wait_for_reader_closed_impl(Some(0.0)) => {
                // Closed-stream is EOF, not a failed session. Preserve shell status.
                let code = process.wait_impl(Some(2.0)).ok();
                if deliver(&process, &tx, exit_event(&id, code)) {
                    return;
                }
                break false;
            }
            Err(_) => break true,
        }
    };
    // A refused delivery means the terminal was stopped, not that the pump failed.
    if failure {
        failed.store(true, Ordering::Relaxed);
    }
    stop_process(&process);
}

fn shell_arguments(isolated: bool) -> Vec<String> {
    if crate::platform::host::WINDOWS {
        let mut argv = vec![std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())];
        if isolated {
            argv.push("/d".into());
        }
        argv
    } else if isolated {
        // Login/interactive rc files can reload a user's provider credentials.
        vec!["/bin/sh".into()]
    } else {
        vec![
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
            "-l".into(),
        ]
    }
}

/// The shell's environment: `base` (a bare `TERM` without one) with
/// `clud_dir` first on PATH. connect.ps1 and connect.sh run a private copy of
/// clud that is on no PATH, so without this `clud` in the operator's shell is
/// not found, or is some other install. The PTY environment replaces the
/// inherited one rather than overlaying it, so this is the shell's whole PATH.
/// The key is matched case-insensitively (Windows `Path`) and an existing
/// entry for the directory is moved to the front, not duplicated.
fn shell_environment(
    base: Option<Vec<(String, String)>>,
    clud_dir: Option<&Path>,
) -> Vec<(String, String)> {
    let mut environment = base.unwrap_or_else(|| vec![("TERM".into(), "xterm-256color".into())]);
    if let Some(dir) = clud_dir {
        crate::shim_session::prepend_to_path(&mut environment, dir);
    }
    environment
}

/// The directory of the running clud, the binary serving this session. A
/// symlink (Homebrew's `bin/clud`) is followed to the real file, so the
/// directory added holds clud rather than every tool beside the link.
/// Windows keeps the module path as it is: resolving it can turn a mapped
/// drive into a UNC path.
fn running_clud_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = if crate::platform::host::WINDOWS {
        exe
    } else {
        crate::path_norm::canonicalize_plain(&exe).unwrap_or(exe)
    };
    exe.parent().map(Path::to_path_buf)
}

impl Terminal {
    pub fn configure_proxy(&mut self, proxy: super::proxy::SessionProxy) -> Result<(), String> {
        let config = tempfile::Builder::new()
            .prefix("clud-remote-auth-")
            .tempdir()
            .map_err(|_| "cannot isolate remote agent credentials")?;
        super::proxy::SessionProxy::seed_agent_config(config.path())
            .map_err(|_| "cannot prepare remote agent configuration")?;
        self.environment = Some(proxy.environment(std::env::vars().collect(), config.path()));
        self.config = Some(config);
        Ok(())
    }

    fn opened(&self) -> Result<Value, String> {
        let pid = self
            .process
            .as_ref()
            .ok_or("remote terminal is closed")?
            .pid()
            .map_err(|e| e.to_string())?;
        Ok(json!({"type":"pty_opened", "pty_id":self.id, "pid":pid, "cmd":self.command}))
    }

    pub fn open(&mut self, event: &Value) -> Result<Value, String> {
        if self.process.is_some() {
            if self.matches(event) {
                return self.opened();
            }
            return Err("a different remote terminal is already open".into());
        }
        let id = event["pty_id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or("invalid remote terminal ID")?
            .to_owned();
        let size = dimensions(event);
        let process = self.spawn(size)?;
        self.failed = Arc::default();
        let (input_tx, input_rx) = mpsc::channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let input_pending = Arc::clone(&pending);
        let (output_tx, output_rx) = mpsc::sync_channel(OUTPUT_FRAMES);
        let writer = Arc::clone(&process);
        let reader = Arc::clone(&process);
        let input_failed = Arc::clone(&self.failed);
        let output_failed = Arc::clone(&self.failed);
        let output_id = id.clone();
        let mode = PasteMode::default();
        let (paste_in, paste_out) = (InputFilter::new(mode.clone()), OutputTracker::new(mode));
        std::thread::spawn(move || {
            input_pump(writer, input_rx, input_pending, input_failed, paste_in);
        });
        std::thread::spawn(move || {
            output_pump(reader, output_tx, output_id, output_failed, paste_out);
        });
        self.process = Some(process);
        self.size = size;
        self.input = Some(InputQueue {
            tx: input_tx,
            pending,
        });
        self.output = Some(output_rx);
        self.id = Some(id);
        self.opened()
    }

    /// Run future shells under the sudo-capable session leader.
    pub fn enable_leader(&mut self, enabled: bool) {
        self.leader_enabled = enabled;
    }

    /// The leader of the shell just opened, for the sudo flow.
    pub fn take_leader(&mut self) -> Option<LeaderStream> {
        self.leader.take()
    }

    /// Start the shell, under `clud-remote-shell` when enabled. The leader
    /// must connect back and prove itself before the shell exists; if it
    /// cannot, the PTY is killed rather than run without its cleanup owner.
    fn spawn(&mut self, (rows, cols): (u16, u16)) -> Result<Arc<NativePtyProcess>, String> {
        let mut argv = shell_arguments(self.environment.is_some());
        self.command = argv[0].clone();
        let mut environment =
            shell_environment(self.environment.clone(), running_clud_dir().as_deref());
        let listener = if self.leader_enabled {
            let listener = LeaderListener::bind()
                .map_err(|e| format!("cannot prepare the remote shell leader: {e}"))?;
            argv.insert(0, listener.program().to_string_lossy().into_owned());
            environment.extend(listener.environment());
            Some(listener)
        } else {
            None
        };
        let process = Arc::new(
            NativePtyProcess::new(argv, None, Some(environment), rows, cols, None)
                .map_err(|e| e.to_string())?,
        );
        process.start_impl().map_err(|e| e.to_string())?;
        if let Some(listener) = listener {
            let pid = || process.pid().ok().flatten();
            match listener.accept(&pid, LEADER_CONNECT) {
                Ok(stream) => self.leader = Some(stream),
                Err(error) => {
                    stop_process(&process);
                    return Err(format!("the remote shell leader did not start: {error}"));
                }
            }
        }
        Ok(process)
    }

    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// The open terminal as a resumed session reports it to the server.
    pub fn report(&self) -> Option<Value> {
        let (rows, cols) = self.size;
        self.id
            .as_ref()
            .map(|id| json!({"pty_id":id, "cols":cols, "rows":rows}))
    }

    pub fn matches(&self, event: &Value) -> bool {
        self.id
            .as_deref()
            .is_some_and(|id| event["pty_id"].as_str() == Some(id))
    }

    pub fn input(&self, data: &[u8]) -> Result<Delivery, String> {
        if data.len() > 4096 {
            return Err("oversized remote terminal input".into());
        }
        Ok(self
            .input
            .as_ref()
            .map_or(Delivery::Queued, |queue| queue.push(data)))
    }

    pub fn resize(&mut self, event: &Value) -> Result<(), String> {
        if let Some(process) = &self.process {
            let (rows, cols) = dimensions(event);
            crate::session::resize_pty(process, rows, cols).map_err(|e| e.to_string())?;
            self.size = (rows, cols);
        }
        Ok(())
    }

    pub fn output(&mut self) -> Result<Option<Message>, String> {
        if self.process.is_none() {
            return Ok(None);
        }
        if self.failed.load(Ordering::Relaxed) {
            return Err("remote terminal pump failed".into());
        }
        let message = self.output.as_ref().and_then(|rx| rx.try_recv().ok());
        if matches!(message, Some(Message::Text(_))) {
            self.stop();
        }
        Ok(message)
    }

    pub fn stop(&mut self) {
        self.input = None;
        self.leader = None;
        if let Some(process) = self.process.take() {
            stop_process(&process);
        }
        self.output = None;
        self.id = None;
    }

    pub fn close_event(&mut self) -> Option<Value> {
        let id = self.id.clone()?;
        self.stop();
        Some(json!({"type":"pty_exit", "pty_id":id, "code":null, "signal":null}))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mouse_report_burst_is_queued_not_fatal() {
        // A pointer swept across the operator's terminal: hundreds of tiny
        // frames while the shell is busy. An 8-frame queue ended the session.
        let (tx, rx) = mpsc::channel();
        let queue = InputQueue {
            tx,
            pending: Arc::default(),
        };
        for col in 0..2000 {
            let report = format!("\x1b[<35;{col};14M");
            assert_eq!(queue.push(report.as_bytes()), Delivery::Queued);
        }
        assert_eq!(rx.try_iter().count(), 2000);
    }

    #[test]
    fn input_past_the_byte_budget_is_dropped_then_flows_again() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let pending = Arc::new(AtomicUsize::new(0));
        let queue = InputQueue {
            tx,
            pending: Arc::clone(&pending),
        };
        let frame = vec![b'x'; 4096];
        let fits = INPUT_BUDGET_BYTES / frame.len();
        for _ in 0..fits {
            assert_eq!(queue.push(&frame), Delivery::Queued);
        }
        assert_eq!(queue.push(&frame), Delivery::Dropped);
        // The pump takes one frame; room for exactly one more.
        let taken = rx.recv().unwrap();
        pending.fetch_sub(taken.len(), Ordering::Relaxed);
        assert_eq!(queue.push(&frame), Delivery::Queued);
        assert_eq!(queue.push(&frame), Delivery::Dropped);
    }
    use std::time::{Duration, Instant};

    #[test]
    fn startup_query_is_removed_once_and_other_output_kept() {
        assert_eq!(
            without_startup_query(b"a\x1b[6nb\x1b[6n").unwrap(),
            b"ab\x1b[6n"
        );
        assert_eq!(without_startup_query(b"\x1b[6n").unwrap(), b"");
        assert_eq!(without_startup_query(b"plain"), None);
    }

    fn read_until(terminal: &mut Terminal, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = String::new();
        while Instant::now() < deadline && !output.contains(needle) {
            match terminal.output().unwrap() {
                Some(Message::Binary(bytes)) => {
                    output.push_str(&String::from_utf8_lossy(&bytes[1..]));
                }
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        output
    }

    /// A paste the operator console wrapped in bracketed-paste markers reaches
    /// a program that never enabled bracketed paste as the plain text, not as
    /// `^[[200~curl ... | sh~` (the clud.bot install one-liner report).
    #[test]
    fn a_bracketed_paste_reaches_a_program_without_the_mode_as_plain_text() {
        if crate::platform::host::WINDOWS {
            return;
        }
        let config = tempfile::tempdir().unwrap();
        let proxy = super::super::proxy::SessionProxy::new(
            "http://127.0.0.1:1234",
            "ct_paste_gate_fixture_1234567890",
            "m",
        )
        .unwrap();
        let mut terminal = Terminal::default();
        terminal.environment = Some(proxy.environment(std::env::vars().collect(), config.path()));
        terminal.open(&json!({"pty_id":"paste-test"})).unwrap();
        // The shell turns bracketed paste off before running a command, so
        // `read` waits with the mode off, as `sudo` or `install.sh` would.
        terminal
            .input(b"stty -echo; printf 'READ%s\\n' Y; IFS= read -r x; printf 'GOT[%s]\\n' \"$x\" | od -c | tr -s ' ' | tr -d '\\n'; printf '\\nEN%s\\n' D\n")
            .unwrap();
        let ready = read_until(&mut terminal, "READY");
        assert!(ready.contains("READY"), "{ready}");
        terminal
            .input(b"\x1b[200~curl -fsSL https://clud.bot/install.sh | sh\x1b[201~\r")
            .unwrap();
        let output = read_until(&mut terminal, "END");
        terminal.stop();
        assert!(output.contains("END"), "{output}");
        assert!(output.contains("G O T [ c u r l"), "{output}");
        assert!(output.contains("| s h ]"), "{output}");
        assert!(
            !output.contains("033"),
            "paste markers reached the program: {output}"
        );
    }

    fn proxied_terminal(id: &str, config: &tempfile::TempDir) -> Terminal {
        let proxy = super::super::proxy::SessionProxy::new(
            "http://127.0.0.1:1234",
            "ct_flood_fixture_123456789012345",
            "m",
        )
        .unwrap();
        let mut terminal = Terminal::default();
        terminal.environment = Some(proxy.environment(std::env::vars().collect(), config.path()));
        terminal.open(&json!({"pty_id":id})).unwrap();
        terminal
    }

    /// Bytes the pump has pulled from the PTY but not yet queued as frames.
    /// `None` while the pump holds the reader's lock: it is waiting for room.
    fn reader_backlog(terminal: &Terminal) -> Option<usize> {
        let process = terminal.process.as_ref().unwrap();
        let state = process.reader.state.try_lock().ok()?;
        Some(state.chunks.iter().map(Vec::len).sum())
    }

    /// Every byte up to `END\r\n`. Checks only the tail for the marker:
    /// [`read_until`] rescans all output per frame, quadratic over megabytes.
    fn drain_until_end(terminal: &mut Terminal) -> String {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            match terminal.output().unwrap() {
                Some(Message::Binary(bytes)) => {
                    output.extend_from_slice(&bytes[1..]);
                    let tail = &output[output.len().saturating_sub(64)..];
                    if tail.windows(5).any(|w| w == b"END\r\n") {
                        break;
                    }
                }
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        String::from_utf8_lossy(&output).into_owned()
    }

    /// `uv` printing heavy progress output while the session loop was slow to
    /// forward it ended the session ("exceeded its output buffer"). A full
    /// queue now slows the shell instead: nothing is lost or reordered, memory
    /// stays bounded, and the shell's exit is still reported afterwards.
    #[test]
    fn an_output_flood_past_a_stalled_consumer_slows_the_shell_and_loses_nothing() {
        if crate::platform::host::WINDOWS {
            return;
        }
        // ~12 MB through the PTY. `awk`, not `seq`: BSD seq prints 1000000 as 1e+06.
        const LINES: usize = 1_500_000;
        let config = tempfile::tempdir().unwrap();
        let mut terminal = proxied_terminal("flood-test", &config);
        terminal
            .input(
                format!("stty -echo; printf 'BEG%s\\n' IN; awk 'BEGIN {{ for (i = 1; i <= {LINES}; i++) print i }}'; printf 'EN%s\\n' D\n")
                    .as_bytes(),
            )
            .unwrap();
        // Forward nothing while the shell floods.
        std::thread::sleep(Duration::from_millis(1500));
        let pulled = terminal.process.as_ref().unwrap().pty_output_bytes_total();
        assert!(
            pulled < 4 * 1024 * 1024,
            "the shell was not slowed: {pulled} bytes pulled from the PTY while stalled"
        );
        if let Some(backlog) = reader_backlog(&terminal) {
            assert!(backlog <= 1024 * 1024, "reader backlog grew to {backlog}");
        }
        let output = drain_until_end(&mut terminal);
        let body = output
            .split_once("BEGIN\r\n")
            .and_then(|(_, rest)| rest.split_once("END\r\n"))
            .map(|(body, _)| body)
            .unwrap_or_else(|| {
                panic!(
                    "markers missing; tail: {:?}",
                    &output[output.len().saturating_sub(200)..]
                )
            });
        let mut expected = 1;
        for line in body.lines() {
            assert_eq!(line.trim_end_matches('\r'), expected.to_string());
            expected += 1;
        }
        assert_eq!(expected, LINES + 1, "lines missing");
        terminal.input(b"exit\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut exited = false;
        while Instant::now() < deadline && !exited {
            match terminal.output().unwrap() {
                Some(Message::Text(_)) => exited = true,
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        assert!(exited, "the shell's exit was not reported after the flood");
    }

    /// Stopping while the pump waits on a full queue neither hangs nor leaves
    /// the reader's lock held.
    #[test]
    fn stop_does_not_hang_on_a_pump_waiting_for_room() {
        if crate::platform::host::WINDOWS {
            return;
        }
        let config = tempfile::tempdir().unwrap();
        let mut terminal = proxied_terminal("flood-stop", &config);
        terminal.input(b"yes flood-output-line\n").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        terminal.stop();
        assert!(started.elapsed() < Duration::from_secs(8), "stop hung");
    }

    /// A host-correct PATH value from its entries.
    fn path_of(entries: &[&str]) -> String {
        std::env::join_paths(entries)
            .unwrap()
            .into_string()
            .unwrap()
    }

    fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn the_clud_directory_is_prepended_and_the_rest_kept_in_order() {
        let base = pairs(&[
            ("HOME", "/home/u"),
            ("PATH", &path_of(&["/usr/local/bin", "/usr/bin", "/bin"])),
            ("TERM", "xterm-256color"),
        ]);
        let env = shell_environment(Some(base), Some(Path::new("/opt/clud")));
        assert_eq!(
            env,
            pairs(&[
                ("HOME", "/home/u"),
                (
                    "PATH",
                    &path_of(&["/opt/clud", "/usr/local/bin", "/usr/bin", "/bin"])
                ),
                ("TERM", "xterm-256color"),
            ])
        );
    }

    /// Windows spells the key `Path`; a second `PATH` entry would leave the
    /// shell with whichever one the environment block happened to keep.
    #[test]
    fn a_differently_cased_path_key_is_updated_in_place() {
        let base = pairs(&[("Path", &path_of(&[r"\Windows\System32"]))]);
        let env = shell_environment(Some(base), Some(Path::new(r"\clud\bin")));
        assert_eq!(
            env,
            pairs(&[("Path", &path_of(&[r"\clud\bin", r"\Windows\System32"]))])
        );
    }

    #[test]
    fn a_directory_already_first_is_not_added_again() {
        let path = path_of(&["/opt/clud", "/usr/bin"]);
        let base = pairs(&[("PATH", &path)]);
        let env = shell_environment(Some(base.clone()), Some(Path::new("/opt/clud")));
        assert_eq!(env, base);
    }

    #[test]
    fn a_directory_later_on_path_moves_first_without_a_duplicate() {
        let base = pairs(&[("PATH", &path_of(&["/usr/bin", "/opt/clud", "/bin"]))]);
        let env = shell_environment(Some(base), Some(Path::new("/opt/clud")));
        assert_eq!(
            env,
            pairs(&[("PATH", &path_of(&["/opt/clud", "/usr/bin", "/bin"]))])
        );
    }

    #[test]
    fn path_is_created_when_the_environment_has_none() {
        let base = pairs(&[("TERM", "xterm-256color")]);
        let env = shell_environment(Some(base), Some(Path::new("/opt/clud")));
        assert_eq!(
            env,
            pairs(&[("TERM", "xterm-256color"), ("PATH", "/opt/clud")])
        );
    }

    #[test]
    fn the_term_only_fallback_also_gets_the_clud_directory() {
        assert_eq!(
            shell_environment(None, Some(Path::new("/opt/clud"))),
            pairs(&[("TERM", "xterm-256color"), ("PATH", "/opt/clud")])
        );
        assert_eq!(
            shell_environment(None, None),
            pairs(&[("TERM", "xterm-256color")])
        );
    }

    #[test]
    fn the_running_clud_directory_holds_the_running_binary() {
        let dir = running_clud_dir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap();
        assert!(dir.join(name).is_file(), "{}", dir.display());
    }

    /// The operator's shell finds the binary serving the session first on
    /// PATH: connect.ps1/connect.sh run a copy that is on no PATH.
    #[test]
    fn the_operator_shell_has_the_running_clud_first_on_path() {
        if crate::platform::host::WINDOWS {
            return; // ConPTY repaints long lines; the e2e test covers Windows.
        }
        let config = tempfile::tempdir().unwrap();
        let mut terminal = proxied_terminal("path-test", &config);
        terminal
            .input(b"printf 'FIRST[%s]%s\\n' \"${PATH%%:*}\" END\n")
            .unwrap();
        // Not in the echoed command, so only the shell's output matches.
        let output = read_until(&mut terminal, "]END");
        terminal.stop();
        let exe = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let expected = format!("FIRST[{}]END", exe.parent().unwrap().display());
        assert!(output.contains(&expected), "want {expected}: {output}");
    }

    #[test]
    fn isolated_shell_disables_profile_startup() {
        let argv = shell_arguments(true);
        if crate::platform::host::WINDOWS {
            assert_eq!(argv.last().map(String::as_str), Some("/d"));
        } else {
            assert_eq!(argv, vec!["/bin/sh"]);
        }
    }

    #[test]
    fn isolated_pty_does_not_read_login_or_env_credential_helpers() {
        if crate::platform::host::WINDOWS {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let profile = home.path().join(".profile");
        std::fs::write(&profile, "echo STARTUP_PROFILE_RAN\n").unwrap();
        let mut ambient: Vec<_> = std::env::vars().filter(|(key, _)| key != "HOME").collect();
        ambient.extend([
            ("HOME".into(), home.path().to_string_lossy().into_owned()),
            ("ENV".into(), profile.to_string_lossy().into_owned()),
            ("BASH_ENV".into(), profile.to_string_lossy().into_owned()),
        ]);
        let proxy = super::super::proxy::SessionProxy::new(
            "http://127.0.0.1:1234",
            "ct_profile_isolation_fixture_123456",
            "m",
        )
        .unwrap();
        let mut terminal = Terminal::default();
        terminal.environment = Some(proxy.environment(ambient, config.path()));
        terminal.open(&json!({"pty_id":"profile-test"})).unwrap();
        terminal
            .input(b"printf 'CLEAN_%s_READY\\n' SHELL\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = String::new();
        while Instant::now() < deadline {
            if let Some(Message::Binary(bytes)) = terminal.output().unwrap() {
                output.push_str(&String::from_utf8_lossy(&bytes[1..]));
                if output.contains("CLEAN_SHELL_READY") {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        terminal.stop();
        assert!(output.contains("CLEAN_SHELL_READY"), "{output}");
        assert!(
            !output.contains("STARTUP_PROFILE_RAN"),
            "profile reloaded credentials: {output}"
        );
    }
}

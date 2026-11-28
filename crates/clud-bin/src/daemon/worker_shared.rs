use std::fs;
use std::io::{self, Write};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use running_process::telemetry::{
    TeeBackpressure, TeeEvent, TeeHandle, TeeOptions, TeeRegistry, TeeStream,
};

use crate::capture::TerminalCapture;
use crate::process_identity;

use super::io_helpers::{read_json_file, write_json_file};
use super::paths::{session_log_path, session_snapshot_path};
#[cfg(test)]
use super::types::DEFAULT_BACKLOG_LIMIT_BYTES;
use super::types::{
    unix_millis_now, AttachClientResult, AttachedClient, BacklogState, CtrlCProfile, SessionKind,
    SessionSnapshot, WorkerServerMessage, LOG_ROTATE_BYTES,
};

use crate::platform::worker_shared::probe_socket_dead;

pub(super) struct WorkerShared {
    pub(super) state_dir: PathBuf,
    pub(super) session_id: String,
    snapshot: Mutex<SessionSnapshot>,
    pub(super) backlog: Mutex<BacklogState>,
    backlog_limit_bytes: usize,
    /// Server-side terminal emulator for PTY sessions. `Some` when the session
    /// kind is `Pty` and issue #34 attach-replay is active. The parser tracks
    /// grid + cursor + alt-screen state so a mid-session attach can emit a
    /// synthesized repaint instead of a raw byte dump, which would leave a
    /// TUI-attached client staring at a garbled frame.
    capture: Mutex<Option<TerminalCapture>>,
    /// Append-only log file for this session. Every output chunk is written
    /// here in addition to the in-memory backlog, so `clud logs <id>` can
    /// pm2-style tail / follow output that has scrolled off the in-memory
    /// backlog or from sessions that have fully exited.
    log_file: Mutex<Option<fs::File>>,
    transcript_tees: TeeRegistry,
    transcript_stream: TeeStream,
    transcript_sink: Mutex<Option<TranscriptSink>>,
    client: Mutex<Option<AttachedClient>>,
    next_client_id: AtomicU64,
    pub(super) stop_accepting: AtomicBool,
    /// #1582: set once any client completes `attach_client`. Until then the
    /// launching client may still be on its way (it can spend seconds on
    /// pre-attach work such as Codex model discovery), so an exited worker
    /// must keep its port open for it; see `awaiting_first_client`.
    ever_attached: AtomicBool,
    /// #1582: when the child's exit was broadcast, bounding how long an
    /// exited worker waits for a first client that may never come
    /// (`--detach`).
    exit_broadcast_at: Mutex<Option<Instant>>,
}

struct TranscriptSink {
    handle: TeeHandle,
    worker: std::thread::JoinHandle<()>,
}

impl WorkerShared {
    #[cfg(test)]
    pub(super) fn new(state_dir: PathBuf, session_id: String, snapshot: SessionSnapshot) -> Self {
        Self::new_with_backlog(state_dir, session_id, snapshot, DEFAULT_BACKLOG_LIMIT_BYTES)
    }

    pub(super) fn new_with_backlog(
        state_dir: PathBuf,
        session_id: String,
        snapshot: SessionSnapshot,
        backlog_limit_bytes: usize,
    ) -> Self {
        let transcript_stream = match &snapshot.kind {
            SessionKind::Pty => TeeStream::PtyOutput,
            SessionKind::Subprocess => TeeStream::Stdout,
        };
        Self {
            state_dir,
            session_id,
            snapshot: Mutex::new(snapshot),
            backlog: Mutex::new(BacklogState::default()),
            backlog_limit_bytes: backlog_limit_bytes.max(1),
            capture: Mutex::new(None),
            log_file: Mutex::new(None),
            transcript_tees: TeeRegistry::new(),
            transcript_stream,
            transcript_sink: Mutex::new(None),
            client: Mutex::new(None),
            next_client_id: AtomicU64::new(1),
            stop_accepting: AtomicBool::new(false),
            ever_attached: AtomicBool::new(false),
            exit_broadcast_at: Mutex::new(None),
        }
    }

    /// Open the session's log file for append. Called once during worker
    /// startup. A failure here is non-fatal: we log a warning and continue
    /// without a log file rather than killing the session.
    pub(super) fn init_log_file(&self) {
        let path = session_log_path(&self.state_dir, &self.session_id);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                *self.log_file.lock().expect("log_file mutex poisoned") = Some(file);
            }
            Err(err) => {
                eprintln!(
                    "[clud] warning: cannot open session log {}: {}",
                    path.display(),
                    err
                );
            }
        }
    }

    /// Append `chunk` to the session log, rotating at the soft size cap.
    fn append_log(&self, chunk: &[u8]) {
        let mut guard = self.log_file.lock().expect("log_file mutex poisoned");
        let Some(file) = guard.as_mut() else { return };
        if file.write_all(chunk).is_err() {
            return;
        }
        let _ = file.flush();
        if let Ok(meta) = file.metadata() {
            if meta.len() >= LOG_ROTATE_BYTES {
                drop(guard);
                self.rotate_log();
            }
        }
    }

    fn rotate_log(&self) {
        let primary = session_log_path(&self.state_dir, &self.session_id);
        let backup = primary.with_extension("log.1");
        // Close the current handle first so Windows lets us rename it.
        {
            let mut guard = self.log_file.lock().expect("log_file mutex poisoned");
            *guard = None;
        }
        let _ = fs::remove_file(&backup);
        if fs::rename(&primary, &backup).is_err() {
            // Rename failed (file in use, etc.) — reopen primary anyway.
        }
        if let Ok(file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&primary)
        {
            *self.log_file.lock().expect("log_file mutex poisoned") = Some(file);
        }
    }

    pub(super) fn init_transcript_file(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        let (handle, receiver) = self.transcript_tees.add_channel_with_options(
            self.transcript_stream,
            1024,
            TeeOptions {
                backpressure: TeeBackpressure::DropOldest,
            },
        );
        let worker = std::thread::Builder::new()
            .name("clud-transcript-tee".into())
            .spawn(move || transcript_file_worker(&mut file, receiver))
            .map_err(io::Error::other)?;
        self.transcript_sink
            .lock()
            .expect("transcript_sink mutex poisoned")
            .replace(TranscriptSink { handle, worker });
        Ok(())
    }

    pub(super) fn close_transcript(&self) {
        let sink = self
            .transcript_sink
            .lock()
            .expect("transcript_sink mutex poisoned")
            .take();
        let Some(sink) = sink else { return };
        let _ = self.transcript_tees.remove(sink.handle);
        if sink.worker.join().is_err() {
            eprintln!("[clud] warning: transcript writer thread panicked");
        }
    }

    /// Activate terminal capture for a PTY session. No-op for subprocess
    /// sessions, whose output is line-oriented and doesn't benefit from grid
    /// replay (and would pay parser cost for nothing).
    pub(super) fn init_capture(&self, rows: u16, cols: u16) {
        *self.capture.lock().expect("capture mutex poisoned") =
            Some(TerminalCapture::new(rows, cols));
    }

    pub(super) fn resize_capture(&self, rows: u16, cols: u16) {
        if let Some(capture) = self
            .capture
            .lock()
            .expect("capture mutex poisoned")
            .as_mut()
        {
            capture.resize(rows, cols);
        }
    }

    pub(super) fn snapshot(&self) -> SessionSnapshot {
        self.snapshot
            .lock()
            .expect("snapshot mutex poisoned")
            .clone()
    }

    pub(super) fn set_root_pid(&self, root_pid: Option<u32>) {
        // Issue #558: capture the start time in the same breath as the PID,
        // and outside the lock -- the process-table walk is the expensive
        // part, and only this method writes `root_pid`.
        let root_pid_start = match root_pid {
            Some(pid) => process_identity::start_time_of(pid),
            None => process_identity::UNKNOWN_START_TIME,
        };
        let snapshot = {
            let mut guard = self.snapshot.lock().expect("snapshot mutex poisoned");
            guard.root_pid = root_pid;
            guard.root_pid_start = root_pid_start;
            guard.clone()
        };
        let _ = self.persist_snapshot(&snapshot);
    }

    pub(super) fn set_exit_code(&self, exit_code: i32) -> i32 {
        let snapshot = {
            let mut guard = self.snapshot.lock().expect("snapshot mutex poisoned");
            if guard.exit_code.is_none() {
                guard.exit_code = Some(exit_code);
                guard.exited_at = Some(unix_millis_now());
            }
            guard.clone()
        };
        let _ = self.persist_snapshot(&snapshot);
        snapshot.exit_code.unwrap_or(exit_code)
    }

    pub(super) fn set_background(&self, background: bool) {
        let snapshot = {
            let mut guard = self.snapshot.lock().expect("snapshot mutex poisoned");
            if guard.background == background {
                return;
            }
            guard.background = background;
            guard.clone()
        };
        let _ = self.persist_snapshot(&snapshot);
    }

    pub(super) fn set_repeat_state(&self, running: bool, next_run_at: Option<u64>) {
        let snapshot = {
            let mut guard = self.snapshot.lock().expect("snapshot mutex poisoned");
            guard.repeat_running = running;
            guard.repeat_next_run_at = next_run_at;
            guard.clone()
        };
        let _ = self.persist_snapshot(&snapshot);
    }

    pub(super) fn record_ctrl_c_handoff(&self, mut profile: CtrlCProfile) {
        profile.fast_path = true;
        let snapshot = {
            let mut guard = self.snapshot.lock().expect("snapshot mutex poisoned");
            let current = guard.ctrl_c.get_or_insert_with(CtrlCProfile::default);
            merge_ctrl_c_profile(current, profile);
            if current.daemon_received_at_ms.is_none() {
                current.daemon_received_at_ms = Some(unix_millis_now());
            }
            current.fast_path = true;
            guard.clone()
        };
        let _ = self.persist_snapshot(&snapshot);
    }

    pub(super) fn record_ctrl_c_kill_started(&self) -> u64 {
        let now = unix_millis_now();
        let snapshot = {
            let mut guard = self.snapshot.lock().expect("snapshot mutex poisoned");
            let current = guard.ctrl_c.get_or_insert_with(CtrlCProfile::default);
            if current.daemon_received_at_ms.is_none() {
                current.daemon_received_at_ms = Some(now);
            }
            if current.daemon_kill_started_at_ms.is_none() {
                current.daemon_kill_started_at_ms = Some(now);
            }
            current.fast_path = true;
            guard.clone()
        };
        let _ = self.persist_snapshot(&snapshot);
        now
    }

    pub(super) fn record_ctrl_c_kill_finished(&self, started_at_ms: u64) {
        let now = unix_millis_now();
        let snapshot = {
            let mut guard = self.snapshot.lock().expect("snapshot mutex poisoned");
            let current = guard.ctrl_c.get_or_insert_with(CtrlCProfile::default);
            if current.daemon_kill_started_at_ms.is_none() {
                current.daemon_kill_started_at_ms = Some(started_at_ms);
            }
            current.daemon_kill_finished_at_ms = Some(now);
            current.daemon_kill_ms = Some(now.saturating_sub(started_at_ms));
            current.fast_path = true;
            guard.clone()
        };
        let _ = self.persist_snapshot(&snapshot);
    }

    pub(super) fn attach_client(&self, shutdown: TcpStream) -> Result<AttachClientResult, String> {
        // First, try to evict any dead client before checking occupancy.
        self.evict_dead_client();
        let mut guard = self.client.lock().expect("client mutex poisoned");
        if guard.is_some() {
            return Err("session already has an attached client".to_string());
        }
        let (tx, rx) = mpsc::channel();
        let client_id = self.next_client_id.fetch_add(1, Ordering::AcqRel);
        guard.replace(AttachedClient {
            id: client_id,
            sender: tx,
            shutdown,
            attached_at: Instant::now(),
        });
        drop(guard);
        self.ever_attached.store(true, Ordering::Release);
        self.set_background(false);
        let snapshot = self.snapshot();
        // For PTY sessions with terminal capture, emit a single synthesized
        // repaint — cells + cursor + alt-screen + mode flags — that reproduces
        // the current display on a fresh terminal. Raw backlog cannot do this
        // for TUIs because cursor moves and partial redraws stack into garbage
        // when played from the middle of a session. See issue #34.
        //
        // For subprocess (line-oriented) sessions we keep the raw-backlog
        // replay: each line is complete on its own and a history dump is
        // what a user attaching mid-run actually wants to see.
        let replay = {
            let capture = self.capture.lock().expect("capture mutex poisoned");
            if let Some(capture) = capture.as_ref() {
                vec![capture.snapshot_bytes()]
            } else {
                self.backlog
                    .lock()
                    .expect("backlog mutex poisoned")
                    .chunks
                    .iter()
                    .cloned()
                    .collect()
            }
        };
        Ok((client_id, rx, snapshot, replay))
    }

    pub(super) fn detach_client(&self, client_id: u64) {
        let mut guard = self.client.lock().expect("client mutex poisoned");
        if guard.as_ref().is_some_and(|client| client.id == client_id) {
            *guard = None;
        }
        drop(guard);
        if self.snapshot().exit_code.is_none() {
            self.set_background(true);
        }
    }

    pub(super) fn owns_client(&self, client_id: u64) -> bool {
        self.client
            .lock()
            .expect("client mutex poisoned")
            .as_ref()
            .is_some_and(|client| client.id == client_id)
    }

    pub(super) fn has_client(&self) -> bool {
        self.client.lock().expect("client mutex poisoned").is_some()
    }

    /// Check if the attached client's TCP connection is still alive.
    /// If the peer has disconnected, evict the dead client so new attaches succeed.
    pub(super) fn evict_dead_client(&self) {
        let mut guard = self.client.lock().expect("client mutex poisoned");
        let should_evict = if let Some(client) = guard.as_ref() {
            probe_socket_dead(&client.shutdown, client.attached_at)
        } else {
            false
        };
        if should_evict {
            if let Some(old) = guard.take() {
                let _ = old.shutdown.shutdown(Shutdown::Both);
            }
            drop(guard);
            if self.snapshot().exit_code.is_none() {
                self.set_background(true);
            }
        }
    }

    pub(super) fn push_output(&self, chunk: Vec<u8>) {
        {
            let mut backlog = self.backlog.lock().expect("backlog mutex poisoned");
            backlog.total_bytes += chunk.len();
            backlog.chunks.push_back(chunk.clone());
            while backlog.total_bytes > self.backlog_limit_bytes {
                if let Some(front) = backlog.chunks.pop_front() {
                    backlog.total_bytes = backlog.total_bytes.saturating_sub(front.len());
                } else {
                    break;
                }
            }
        }
        // Feed the server-side terminal emulator so the grid stays in sync
        // with what the backend is rendering. Cheap enough to do on the hot
        // path (vt100 is a streaming VTE-based parser); the expensive part,
        // snapshot synthesis, only runs on attach.
        if let Some(capture) = self
            .capture
            .lock()
            .expect("capture mutex poisoned")
            .as_mut()
        {
            capture.feed(&chunk);
        }
        // pm2-style persistent log: every byte that the session produces gets
        // appended to `<state_dir>/logs/<session_id>.log`, so `clud logs` can
        // show output that scrolled off the 256 KiB in-memory backlog.
        self.append_log(&chunk);
        self.transcript_tees.write(self.transcript_stream, &chunk);
        self.send_to_client(WorkerServerMessage::Output {
            data_b64: base64::engine::general_purpose::STANDARD.encode(chunk),
        });
    }

    pub(super) fn broadcast_exit(&self, exit_code: i32) {
        let exit_code = self.set_exit_code(exit_code);
        self.exit_broadcast_at
            .lock()
            .expect("exit_broadcast_at mutex poisoned")
            .get_or_insert_with(Instant::now);
        self.stop_accepting.store(true, Ordering::Release);
        self.send_to_client(WorkerServerMessage::Exited { exit_code });
    }

    /// #1582: true while no client has ever attached and the child exited
    /// less than `grace` ago. The exit code and backlog live in this process,
    /// so a worker that quit here would turn a successful short session into
    /// a client-side "worker has died" failure for the client still
    /// connecting to it.
    pub(super) fn awaiting_first_client(&self, grace: Duration) -> bool {
        if self.ever_attached.load(Ordering::Acquire) {
            return false;
        }
        match *self
            .exit_broadcast_at
            .lock()
            .expect("exit_broadcast_at mutex poisoned")
        {
            Some(at) => at.elapsed() < grace,
            None => true,
        }
    }

    pub(super) fn send_to_client(&self, message: WorkerServerMessage) {
        let sender = self
            .client
            .lock()
            .expect("client mutex poisoned")
            .as_ref()
            .map(|client| client.sender.clone());
        if let Some(sender) = sender {
            let _ = sender.send(message);
        }
    }

    pub(super) fn persist_current_snapshot(&self) -> io::Result<()> {
        let snapshot = self.snapshot();
        self.persist_snapshot(&snapshot)
    }

    fn persist_snapshot(&self, snapshot: &SessionSnapshot) -> io::Result<()> {
        let path = session_snapshot_path(&self.state_dir, &self.session_id);
        let mut snapshot = snapshot.clone();
        if let Ok(current) = read_json_file::<SessionSnapshot>(&path) {
            if let Some(profile) = current.ctrl_c {
                let target = snapshot.ctrl_c.get_or_insert_with(CtrlCProfile::default);
                let fast_path = profile.fast_path;
                merge_ctrl_c_profile(target, profile);
                if fast_path && current.exit_code.is_some() {
                    snapshot.exit_code = current.exit_code;
                    snapshot.background = current.background;
                }
            }
        }
        write_json_file(&path, &snapshot)
    }
}

fn merge_ctrl_c_profile(current: &mut CtrlCProfile, update: CtrlCProfile) {
    if update.cli_pid.is_some() {
        current.cli_pid = update.cli_pid;
    }
    if update.cli_observed_at_ms.is_some() {
        current.cli_observed_at_ms = update.cli_observed_at_ms;
    }
    if update.cli_handoff_at_ms.is_some() {
        current.cli_handoff_at_ms = update.cli_handoff_at_ms;
    }
    if update.cli_return_ready_at_ms.is_some() {
        current.cli_return_ready_at_ms = update.cli_return_ready_at_ms;
    }
    if update.cli_handoff_ms.is_some() {
        current.cli_handoff_ms = update.cli_handoff_ms;
    }
    if update.daemon_received_at_ms.is_some() {
        current.daemon_received_at_ms = update.daemon_received_at_ms;
    }
    if update.daemon_kill_started_at_ms.is_some() {
        current.daemon_kill_started_at_ms = update.daemon_kill_started_at_ms;
    }
    if update.daemon_kill_finished_at_ms.is_some() {
        current.daemon_kill_finished_at_ms = update.daemon_kill_finished_at_ms;
    }
    if update.daemon_kill_ms.is_some() {
        current.daemon_kill_ms = update.daemon_kill_ms;
    }
    current.fast_path |= update.fast_path;
}

impl Drop for WorkerShared {
    fn drop(&mut self) {
        self.close_transcript();
    }
}

fn transcript_file_worker(file: &mut fs::File, receiver: mpsc::Receiver<TeeEvent>) {
    while let Ok(event) = receiver.recv() {
        let result = match event {
            TeeEvent::Bytes(bytes) => file.write_all(&bytes),
            TeeEvent::MissedBytes(n) => file.write_all(&transcript_missed_marker(n)),
        };
        if result.is_err() || file.flush().is_err() {
            break;
        }
    }
    let _ = file.flush();
}

fn transcript_missed_marker(n: u64) -> Vec<u8> {
    format!("\n[running-process tee missed {n} bytes]\n").into_bytes()
}

#[cfg(test)]
mod tests {
    //! Tests that the terminal-capture layer is wired correctly through
    //! `WorkerShared`. Complements the pure-unit tests in `capture.rs`: those
    //! prove the parser can round-trip any given byte stream; these prove the
    //! daemon calls `init_capture`, `feed`, `resize`, and `snapshot_bytes` at
    //! the right points so an attach actually delivers the current screen.
    use super::*;
    use crate::daemon::types::SessionKind;
    use kernal_api::platform::fs::TemporaryDirectory;
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::Arc;
    use std::thread;

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local_addr").port();
        let client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (client, server)
    }

    fn test_shared(state_dir: &Path, kind: SessionKind) -> Arc<WorkerShared> {
        let snap = SessionSnapshot {
            id: "test-session".into(),
            kind,
            backend: None,
            launch_mode: None,
            repo_root: None,
            command: Vec::new(),
            cwd: None,
            name: None,
            created_at: Some(0),
            detachable: true,
            background: false,
            attachable: true,
            repeat_interval_secs: None,
            repeat_next_run_at: None,
            repeat_running: false,
            daemon_pid: 0,
            worker_pid: 0,
            worker_port: 0,
            root_pid: None,
            daemon_pid_start: 0,
            worker_pid_start: 0,
            root_pid_start: 0,
            exit_code: None,
            exited_at: None,
            ctrl_c: None,
        };
        Arc::new(WorkerShared::new(
            state_dir.to_path_buf(),
            "test-session".into(),
            snap,
        ))
    }

    fn shared_with_log(tmp: &TemporaryDirectory, id: &str) -> Arc<WorkerShared> {
        let snap = SessionSnapshot {
            id: id.into(),
            kind: SessionKind::Subprocess,
            backend: None,
            launch_mode: None,
            repo_root: None,
            command: Vec::new(),
            cwd: None,
            name: None,
            created_at: Some(0),
            detachable: true,
            background: false,
            attachable: true,
            repeat_interval_secs: None,
            repeat_next_run_at: None,
            repeat_running: false,
            daemon_pid: 0,
            worker_pid: 0,
            worker_port: 0,
            root_pid: None,
            daemon_pid_start: 0,
            worker_pid_start: 0,
            root_pid_start: 0,
            exit_code: None,
            exited_at: None,
            ctrl_c: None,
        };
        let shared = Arc::new(WorkerShared::new(tmp.path().to_path_buf(), id.into(), snap));
        shared.init_log_file();
        shared
    }

    #[test]
    fn pty_attach_returns_single_synthesized_snapshot() {
        let tmp = TemporaryDirectory::new().expect("tempdir");
        let shared = test_shared(tmp.path(), SessionKind::Pty);
        shared.init_capture(24, 80);
        shared.push_output(b"\x1b[1;1HHEADER\x1b[5;1HFOOTER".to_vec());

        let (_client, server) = loopback_pair();
        let (_id, _rx, _snap, replay) = shared.attach_client(server).expect("attach");

        assert_eq!(
            replay.len(),
            1,
            "PTY attach must deliver exactly one synthesized snapshot chunk"
        );
        let mut p = vt100::Parser::new(24, 80, 0);
        p.process(&replay[0]);
        let contents = p.screen().contents();
        assert!(contents.contains("HEADER"), "HEADER missing from replay");
        assert!(contents.contains("FOOTER"), "FOOTER missing from replay");
    }

    #[test]
    fn subprocess_attach_returns_raw_backlog_unchanged() {
        // Back-compat guarantee: subprocess sessions are line-oriented, users
        // attaching mid-run want history, not a repaint. `init_capture` is
        // never called in that code path, so `attach_client` must fall back
        // to the raw-chunk replay that pre-dated issue #34.
        let tmp = TemporaryDirectory::new().expect("tempdir");
        let shared = test_shared(tmp.path(), SessionKind::Subprocess);
        shared.push_output(b"line1\n".to_vec());
        shared.push_output(b"line2\n".to_vec());

        let (_client, server) = loopback_pair();
        let (_id, _rx, _snap, replay) = shared.attach_client(server).expect("attach");
        assert_eq!(replay, vec![b"line1\n".to_vec(), b"line2\n".to_vec()]);
    }

    #[test]
    fn reattach_after_detach_delivers_current_frame() {
        // Output arriving *between* detach and reattach must still make it
        // into the second snapshot — the capture keeps feeding even with no
        // client connected.
        let tmp = TemporaryDirectory::new().expect("tempdir");
        let shared = test_shared(tmp.path(), SessionKind::Pty);
        shared.init_capture(24, 80);
        shared.push_output(b"\x1b[1;1HBEFORE".to_vec());

        let (_c1, s1) = loopback_pair();
        let (cid1, _, _, _) = shared.attach_client(s1).expect("first attach");
        shared.detach_client(cid1);

        shared.push_output(b"\x1b[2;1HAFTER".to_vec());

        let (_c2, s2) = loopback_pair();
        let (_, _, _, replay) = shared.attach_client(s2).expect("second attach");
        let mut p = vt100::Parser::new(24, 80, 0);
        p.process(&replay[0]);
        let c = p.screen().contents();
        assert!(c.contains("BEFORE"), "pre-detach content missing: {:?}", c);
        assert!(c.contains("AFTER"), "post-detach content missing: {:?}", c);
    }

    #[test]
    fn live_attached_client_blocks_second_attach() {
        let tmp = TemporaryDirectory::new().expect("tempdir");
        let shared = test_shared(tmp.path(), SessionKind::Subprocess);

        let (_client1, server1) = loopback_pair();
        let (_cid1, _, _, _) = shared.attach_client(server1).expect("first attach");
        thread::sleep(Duration::from_millis(1050));

        let (_client2, server2) = loopback_pair();
        let err = shared.attach_client(server2).expect_err("second attach");
        assert_eq!(err, "session already has an attached client");
    }

    #[test]
    fn dead_attached_client_is_evicted_on_next_attach() {
        let tmp = TemporaryDirectory::new().expect("tempdir");
        let shared = test_shared(tmp.path(), SessionKind::Subprocess);

        let (client1, server1) = loopback_pair();
        let (_cid1, _, _, _) = shared.attach_client(server1).expect("first attach");
        drop(client1);
        thread::sleep(Duration::from_millis(50));

        let (_client2, server2) = loopback_pair();
        shared.attach_client(server2).expect("reattach");
    }

    #[test]
    fn resize_capture_takes_effect_for_subsequent_paint() {
        // Pre-resize the parser grid isn't wide enough to hold col 100.
        // After resize, a paint at col 100 lands correctly and the attach
        // replay reflects it.
        let tmp = TemporaryDirectory::new().expect("tempdir");
        let shared = test_shared(tmp.path(), SessionKind::Pty);
        shared.init_capture(24, 80);
        shared.resize_capture(40, 120);
        shared.push_output(b"\x1b[1;100HEDGE".to_vec());

        let (_c, s) = loopback_pair();
        let (_, _, _, replay) = shared.attach_client(s).expect("attach");

        let mut p = vt100::Parser::new(40, 120, 0);
        p.process(&replay[0]);
        let cell = p.screen().cell(0, 99).expect("cell 0,99 in 120-col grid");
        assert_eq!(
            cell.contents(),
            "E",
            "'E' of EDGE should land at col 99 after resize"
        );
    }

    #[test]
    fn push_output_appends_to_log_file() {
        let tmp = TemporaryDirectory::new().unwrap();
        let shared = shared_with_log(&tmp, "s1");
        shared.push_output(b"line one\n".to_vec());
        shared.push_output(b"line two\n".to_vec());
        // Drop so the file flushes / releases on Windows.
        drop(shared);

        let path = session_log_path(tmp.path(), "s1");
        let contents = fs::read(&path).expect("read log");
        assert_eq!(contents, b"line one\nline two\n");
    }

    #[test]
    fn ctrl_c_profile_records_handoff_and_daemon_kill_timings() {
        let tmp = TemporaryDirectory::new().unwrap();
        let shared = test_shared(tmp.path(), SessionKind::Subprocess);
        shared.record_ctrl_c_handoff(CtrlCProfile {
            cli_pid: Some(123),
            cli_observed_at_ms: Some(1000),
            cli_handoff_at_ms: Some(1015),
            cli_return_ready_at_ms: Some(1015),
            cli_handoff_ms: Some(15),
            fast_path: true,
            ..CtrlCProfile::default()
        });
        let started_at_ms = shared.record_ctrl_c_kill_started();
        shared.record_ctrl_c_kill_finished(started_at_ms);

        let profile = shared.snapshot().ctrl_c.expect("ctrl-c profile");
        assert_eq!(profile.cli_pid, Some(123));
        assert_eq!(profile.cli_handoff_ms, Some(15));
        assert!(profile.daemon_received_at_ms.is_some());
        assert!(profile.daemon_kill_started_at_ms.is_some());
        assert!(profile.daemon_kill_finished_at_ms.is_some());
        assert!(profile.daemon_kill_ms.is_some());
        assert!(profile.fast_path);
    }

    #[test]
    fn worker_exit_preserves_daemon_written_ctrl_c_snapshot() {
        let tmp = TemporaryDirectory::new().unwrap();
        let shared = test_shared(tmp.path(), SessionKind::Subprocess);
        shared.persist_current_snapshot().unwrap();

        let path = session_snapshot_path(tmp.path(), "test-session");
        let mut daemon_snapshot = shared.snapshot();
        daemon_snapshot.exit_code = Some(130);
        daemon_snapshot.background = false;
        daemon_snapshot.ctrl_c = Some(CtrlCProfile {
            cli_pid: Some(321),
            cli_handoff_ms: Some(12),
            daemon_received_at_ms: Some(100),
            fast_path: true,
            ..CtrlCProfile::default()
        });
        write_json_file(&path, &daemon_snapshot).unwrap();

        shared.set_exit_code(1);

        let persisted: SessionSnapshot = read_json_file(&path).unwrap();
        assert_eq!(persisted.exit_code, Some(130));
        assert!(!persisted.background);
        let profile = persisted.ctrl_c.expect("ctrl-c profile should survive");
        assert_eq!(profile.cli_pid, Some(321));
        assert_eq!(profile.cli_handoff_ms, Some(12));
        assert!(profile.fast_path);
    }

    #[test]
    fn transcript_file_receives_output_through_running_process_tee() {
        let tmp = TemporaryDirectory::new().unwrap();
        let shared = test_shared(tmp.path(), SessionKind::Pty);
        let transcript = tmp.path().join("session.transcript");
        shared.init_transcript_file(&transcript).unwrap();
        shared.push_output(b"hello transcript\n".to_vec());
        shared.close_transcript();

        let contents = fs::read(&transcript).expect("read transcript");
        assert_eq!(contents, b"hello transcript\n");
    }

    #[test]
    fn rotation_moves_oversize_log_to_backup() {
        let tmp = TemporaryDirectory::new().unwrap();
        let shared = shared_with_log(&tmp, "s2");
        let chunk = vec![b'x'; (LOG_ROTATE_BYTES / 4) as usize];
        // Push enough chunks to exceed the rotate threshold.
        for _ in 0..6 {
            shared.push_output(chunk.clone());
        }
        drop(shared);

        let primary = session_log_path(tmp.path(), "s2");
        let backup = primary.with_extension("log.1");
        assert!(
            backup.exists(),
            "rotation should have produced a .log.1 backup"
        );
        // Primary may exist (post-rotation reopened) but be smaller than the cap.
        if primary.exists() {
            let len = fs::metadata(&primary).unwrap().len();
            assert!(len < LOG_ROTATE_BYTES, "primary grew past the rotation cap");
        }
    }

    #[test]
    fn worker_shared_honors_backlog_override() {
        // With a 10-byte cap, pushing three 5-byte chunks should evict the
        // oldest so only the most-recent chunks remain resident.
        let tmp = TemporaryDirectory::new().unwrap();
        let snap = SessionSnapshot {
            id: "s".into(),
            kind: SessionKind::Subprocess,
            backend: None,
            launch_mode: None,
            repo_root: None,
            command: Vec::new(),
            cwd: None,
            name: None,
            created_at: Some(0),
            detachable: false,
            background: false,
            attachable: true,
            repeat_interval_secs: None,
            repeat_next_run_at: None,
            repeat_running: false,
            daemon_pid: 0,
            worker_pid: 0,
            worker_port: 0,
            root_pid: None,
            daemon_pid_start: 0,
            worker_pid_start: 0,
            root_pid_start: 0,
            exit_code: None,
            exited_at: None,
            ctrl_c: None,
        };
        let shared = Arc::new(WorkerShared::new_with_backlog(
            tmp.path().to_path_buf(),
            "s".into(),
            snap,
            10,
        ));
        shared.push_output(b"AAAAA".to_vec());
        shared.push_output(b"BBBBB".to_vec());
        shared.push_output(b"CCCCC".to_vec());
        let backlog = shared.backlog.lock().unwrap();
        assert!(
            backlog.total_bytes <= 10,
            "backlog exceeded cap: {} > 10",
            backlog.total_bytes
        );
    }
}

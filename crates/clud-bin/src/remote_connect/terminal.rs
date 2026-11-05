//! Bounded terminal pumps over the existing running-process PTY boundary.
use running_process::pty::NativePtyProcess;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use tungstenite::Message;

const FRAME_BYTES: usize = 16 * 1024;
const OUTPUT_FRAMES: usize = 128;

#[derive(Default)]
pub(super) struct Terminal {
    process: Option<Arc<NativePtyProcess>>,
    id: Option<String>,
    input: Option<mpsc::SyncSender<Vec<u8>>>,
    output: Option<mpsc::Receiver<Message>>,
    failed: Arc<AtomicBool>,
    command: String,
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

fn input_pump(
    process: Arc<NativePtyProcess>,
    rx: mpsc::Receiver<Vec<u8>>,
    failed: Arc<AtomicBool>,
) {
    for bytes in rx {
        if process.write_impl(&bytes, false).is_err() {
            if !process.wait_for_reader_closed_impl(Some(0.0)) {
                failed.store(true, Ordering::Relaxed);
            }
            break;
        }
    }
}

fn output_pump(
    process: Arc<NativePtyProcess>,
    tx: mpsc::SyncSender<Message>,
    id: String,
    failed: Arc<AtomicBool>,
) {
    loop {
        // running-process owns the reader. Fail closed if its queue grows ahead
        // of this bounded pump, rather than retaining an unlimited noisy shell.
        let overflow = process
            .reader
            .state
            .lock()
            .expect("PTY reader lock")
            .chunks
            .len()
            > 64;
        if overflow {
            break;
        }
        match process.read_chunk_impl(Some(0.1)) {
            Ok(Some(bytes)) => {
                for chunk in bytes.chunks(FRAME_BYTES) {
                    let mut frame = vec![0];
                    frame.extend_from_slice(chunk);
                    if tx.try_send(Message::Binary(frame.into())).is_err() {
                        failed.store(true, Ordering::Relaxed);
                        stop_process(&process);
                        return;
                    }
                }
            }
            Ok(None) => {}
            Err(_) if process.wait_for_reader_closed_impl(Some(0.0)) => {
                // Closed-stream is EOF, not a failed session. Preserve shell status.
                let code = process.wait_impl(Some(2.0)).ok();
                let event = json!({"type":"pty_exit", "pty_id":id, "code":code, "signal":null});
                if tx.try_send(Message::Text(event.to_string().into())).is_ok() {
                    return;
                }
                break;
            }
            Err(_) => break,
        }
    }
    failed.store(true, Ordering::Relaxed);
    stop_process(&process);
}

impl Terminal {
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
        let (rows, cols) = dimensions(event);
        let argv = if crate::platform::host::WINDOWS {
            vec![std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())]
        } else {
            vec![
                std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
                "-l".into(),
            ]
        };
        self.command = argv[0].clone();
        let process = Arc::new(
            NativePtyProcess::new(
                argv,
                None,
                Some(vec![("TERM".into(), "xterm-256color".into())]),
                rows,
                cols,
                None,
            )
            .map_err(|e| e.to_string())?,
        );
        process.start_impl().map_err(|e| e.to_string())?;
        self.failed = Arc::default();
        let (input_tx, input_rx) = mpsc::sync_channel(8);
        let (output_tx, output_rx) = mpsc::sync_channel(OUTPUT_FRAMES);
        let writer = Arc::clone(&process);
        let reader = Arc::clone(&process);
        let input_failed = Arc::clone(&self.failed);
        let output_failed = Arc::clone(&self.failed);
        let output_id = id.clone();
        std::thread::spawn(move || input_pump(writer, input_rx, input_failed));
        std::thread::spawn(move || output_pump(reader, output_tx, output_id, output_failed));
        self.process = Some(process);
        self.input = Some(input_tx);
        self.output = Some(output_rx);
        self.id = Some(id);
        self.opened()
    }

    pub fn matches(&self, event: &Value) -> bool {
        self.id
            .as_deref()
            .is_some_and(|id| event["pty_id"].as_str() == Some(id))
    }

    pub fn input(&self, data: &[u8]) -> Result<(), String> {
        if data.len() > 4096 {
            return Err("oversized remote terminal input".into());
        }
        if let Some(tx) = &self.input {
            tx.try_send(data.to_vec())
                .map_err(|_| "remote terminal input buffer is full or closed")?;
        }
        Ok(())
    }

    pub fn resize(&self, event: &Value) -> Result<(), String> {
        if let Some(process) = &self.process {
            let (rows, cols) = dimensions(event);
            crate::session::resize_pty(process, rows, cols).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn output(&mut self) -> Result<Option<Message>, String> {
        if self.process.is_none() {
            return Ok(None);
        }
        if self.failed.load(Ordering::Relaxed) {
            return Err("remote terminal pump failed or exceeded its output buffer".into());
        }
        let message = self.output.as_ref().and_then(|rx| rx.try_recv().ok());
        if matches!(message, Some(Message::Text(_))) {
            self.stop();
        }
        Ok(message)
    }

    pub fn stop(&mut self) {
        self.input = None;
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

#![cfg(target_os = "linux")]

use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ProcessError, StderrMode, StdinMode,
};

pub(super) struct LinuxCapture {
    process: NativeProcess,
    capture_dir: PathBuf,
    reader_done: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<Result<(), String>>>,
}

impl LinuxCapture {
    pub(super) fn start(samples: Arc<Mutex<Vec<f32>>>) -> Result<Self, String> {
        let (capture_dir, sample_path) = create_arecord_output_path()?;
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec![
                "arecord".to_string(),
                "-q".to_string(),
                "-t".to_string(),
                "raw".to_string(),
                "-f".to_string(),
                "S16_LE".to_string(),
                "-c".to_string(),
                "1".to_string(),
                "-r".to_string(),
                "16000".to_string(),
                sample_path.to_string_lossy().into_owned(),
            ]),
            cwd: None,
            env: None,
            capture: true,
            stderr_mode: StderrMode::Pipe,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Null,
            nice: None,
            address_space_limit_bytes: None,
        });

        if let Err(err) = process.start() {
            let _ = fs::remove_dir_all(&capture_dir);
            return Err(match err {
                ProcessError::Spawn(err) if err.kind() == io::ErrorKind::NotFound => {
                    "Linux voice capture requires `arecord` (install alsa-utils)".to_string()
                }
                other => format!("failed to start Linux voice capture (`arecord`): {other}"),
            });
        };

        let reader_done = Arc::new(AtomicBool::new(false));
        let reader = {
            let reader_done = Arc::clone(&reader_done);
            std::thread::spawn(move || read_arecord_file(sample_path, reader_done, samples))
        };

        Ok(Self {
            process,
            capture_dir,
            reader_done,
            reader: Some(reader),
        })
    }

    pub(super) fn stop(mut self) -> Result<(), String> {
        if self
            .process
            .poll()
            .map_err(|err| format!("failed to inspect arecord process: {err}"))?
            .is_none()
        {
            self.process
                .kill()
                .map_err(|err| format!("failed to stop arecord process: {err}"))?;
        }
        let exit_code = self
            .process
            .wait(Some(Duration::from_secs(2)))
            .map_err(|err| format!("failed to wait for arecord process: {err}"))?;

        self.reader_done.store(true, Ordering::Release);
        let reader_result = self
            .reader
            .take()
            .expect("arecord reader thread exists")
            .join()
            .map_err(|_| "arecord reader thread panicked".to_string())?;

        let stderr_text = captured_stderr_text(&self.process);
        let _ = fs::remove_dir_all(&self.capture_dir);

        reader_result?;

        if exit_code <= 0 {
            return Ok(());
        }

        let detail = stderr_text.trim();
        if detail.is_empty() {
            Err(format!(
                "microphone capture command failed with exit code {exit_code}"
            ))
        } else {
            Err(format!("microphone capture command failed: {detail}"))
        }
    }
}

fn create_arecord_output_path() -> Result<(PathBuf, PathBuf), String> {
    let base = env::temp_dir();
    let pid = std::process::id();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for attempt in 0..32_u8 {
        let dir = base.join(format!("clud-arecord-{pid}-{nonce}-{attempt}"));
        match fs::create_dir(&dir) {
            Ok(()) => {
                let sample_path = dir.join("capture.raw");
                return Ok((dir, sample_path));
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(format!(
                    "failed to create temporary audio capture directory: {err}"
                ));
            }
        }
    }

    Err("failed to allocate temporary audio capture path".to_string())
}

fn read_arecord_file(
    path: PathBuf,
    done: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<f32>>>,
) -> Result<(), String> {
    let mut file = loop {
        match fs::File::open(&path) {
            Ok(file) => break file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if done.load(Ordering::Acquire) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => {
                return Err(format!(
                    "failed to open arecord sample file {}: {err}",
                    path.display()
                ));
            }
        }
    };

    let mut buffer = [0u8; 8192];
    let mut carry: Option<u8> = None;

    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|err| format!("failed to read microphone samples: {err}"))?;
        if n == 0 {
            if done.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }

        append_pcm16le_samples(&buffer[..n], &mut carry, &samples)?;
    }

    Ok(())
}

fn append_pcm16le_samples(
    data: &[u8],
    carry: &mut Option<u8>,
    samples: &Arc<Mutex<Vec<f32>>>,
) -> Result<(), String> {
    let mut start = 0usize;
    let mut converted: Vec<f32> = Vec::with_capacity(data.len().div_ceil(2));
    if let Some(lo) = carry.take() {
        if let Some(&hi) = data.first() {
            let sample = i16::from_le_bytes([lo, hi]);
            converted.push(sample as f32 / i16::MAX as f32);
            start = 1;
        } else {
            *carry = Some(lo);
            return Ok(());
        }
    }

    let chunk = &data[start..];
    let even_len = chunk.len() & !1usize;
    for bytes in chunk[..even_len].as_chunks::<2>().0 {
        let sample = i16::from_le_bytes([bytes[0], bytes[1]]);
        converted.push(sample as f32 / i16::MAX as f32);
    }
    if even_len < chunk.len() {
        *carry = Some(chunk[even_len]);
    }

    if !converted.is_empty() {
        samples
            .lock()
            .map_err(|_| "microphone sample buffer lock poisoned".to_string())?
            .extend(converted);
    }

    Ok(())
}

fn captured_stderr_text(process: &NativeProcess) -> String {
    process
        .captured_stderr()
        .into_iter()
        .map(|line| String::from_utf8_lossy(&line).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

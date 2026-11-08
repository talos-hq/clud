use super::*;

/// Stdin another entry already consumed; see [`super::run_for_payload`].
pub(super) static PRELOADED_STDIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub(super) fn read_stdin_bounded() -> StdinRead {
    if let Some(text) = PRELOADED_STDIN.get() {
        return StdinRead {
            text: text.clone(),
            log_messages: Vec::new(),
            incomplete: None,
        };
    }
    if let Some(read) = read_stdin_nonblocking() {
        return read;
    }
    read_stdin_threaded()
}

/// Poll a non-blocking stdin against the deadlines. `None` where stdin
/// cannot be made non-blocking (always on Windows), so the caller uses the
/// threaded reader.
pub(super) fn read_stdin_nonblocking() -> Option<StdinRead> {
    let idle_timeout = float_env_duration(
        "CLUD_HOOK_STDIN_IDLE_TIMEOUT_SEC",
        DEFAULT_STDIN_READ_IDLE_TIMEOUT_SEC,
    );
    let deadline_timeout = float_env_duration(
        "CLUD_HOOK_STDIN_DEADLINE_SEC",
        DEFAULT_STDIN_READ_DEADLINE_SEC,
    );

    let stdin = io::stdin();
    let mut stream = stdin.lock();
    let nonblocking = crate::platform::block_bad_cmd_io::NonblockingStdin::enable(&stream)?;

    let mut chunks = Vec::<u8>::new();
    let mut log_messages = Vec::<String>::new();
    let deadline = Instant::now() + deadline_timeout;
    let mut idle_until: Option<Instant> = None;
    let mut incomplete_reason: Option<&'static str> = None;
    loop {
        let mut buf = [0u8; STDIN_READ_CHUNK_BYTES];
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                chunks.extend_from_slice(&buf[..n]);
                idle_until = Some(Instant::now() + idle_timeout);
                if chunks.len() >= STDIN_READ_MAX_BYTES {
                    incomplete_reason = Some("max_bytes");
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let now = Instant::now();
                let wait_until = idle_until.map_or(deadline, |idle| idle.min(deadline));
                if now >= wait_until {
                    incomplete_reason = Some(if idle_until.is_some_and(|idle| idle <= deadline) {
                        "idle"
                    } else {
                        "deadline"
                    });
                    break;
                }
                std::thread::sleep((wait_until - now).min(Duration::from_millis(10)));
            }
            Err(error) => {
                log_messages.push(format!("stdin_read_error mode=nonblocking error={error}"));
                break;
            }
        }
    }

    nonblocking.restore();
    if let Some(reason) = incomplete_reason {
        log_messages.push(format!(
            "stdin_read_incomplete mode=nonblocking reason={reason} bytes={}",
            chunks.len()
        ));
    }
    Some(StdinRead {
        text: decode_stdin(&chunks),
        log_messages,
        incomplete: incomplete_reason,
    })
}

pub(super) fn read_stdin_threaded() -> StdinRead {
    enum Item {
        Chunk(Vec<u8>),
        Eof,
        Error(String),
    }

    let idle_timeout = float_env_duration(
        "CLUD_HOOK_STDIN_IDLE_TIMEOUT_SEC",
        DEFAULT_STDIN_READ_IDLE_TIMEOUT_SEC,
    );
    let deadline_timeout = float_env_duration(
        "CLUD_HOOK_STDIN_DEADLINE_SEC",
        DEFAULT_STDIN_READ_DEADLINE_SEC,
    );
    let (tx, rx) = mpsc::channel::<Item>();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut stream = stdin.lock();
        loop {
            let mut buf = vec![0u8; STDIN_READ_CHUNK_BYTES];
            match stream.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(Item::Eof);
                    return;
                }
                Ok(n) => {
                    buf.truncate(n);
                    if tx.send(Item::Chunk(buf)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Item::Error(error.to_string()));
                    return;
                }
            }
        }
    });

    let mut chunks = Vec::<u8>::new();
    let mut log_messages = Vec::<String>::new();
    let deadline = Instant::now() + deadline_timeout;
    let mut idle_until: Option<Instant> = None;
    let mut incomplete_reason: Option<&'static str> = None;
    loop {
        let now = Instant::now();
        let wait_until = idle_until.map_or(deadline, |idle| idle.min(deadline));
        if now >= wait_until {
            incomplete_reason = Some(if idle_until.is_some_and(|idle| idle <= deadline) {
                "idle"
            } else {
                "deadline"
            });
            break;
        }
        match rx.recv_timeout(wait_until - now) {
            Ok(Item::Eof) => break,
            Ok(Item::Error(error)) => {
                log_messages.push(format!("stdin_read_error mode=threaded error={error}"));
                break;
            }
            Ok(Item::Chunk(chunk)) => {
                chunks.extend_from_slice(&chunk);
                idle_until = Some(Instant::now() + idle_timeout);
                if chunks.len() >= STDIN_READ_MAX_BYTES {
                    incomplete_reason = Some("max_bytes");
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                incomplete_reason = Some(if idle_until.is_some_and(|idle| idle <= deadline) {
                    "idle"
                } else {
                    "deadline"
                });
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if let Some(reason) = incomplete_reason {
        log_messages.push(format!(
            "stdin_read_incomplete mode=threaded reason={reason} bytes={}",
            chunks.len()
        ));
    }
    StdinRead {
        text: decode_stdin(&chunks),
        log_messages,
        incomplete: incomplete_reason,
    }
}

pub(super) fn decode_stdin(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_start_matches('\u{feff}')
        .to_string()
}

pub(super) fn float_env_duration(name: &str, default: f64) -> Duration {
    let seconds = std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .unwrap_or(default)
        .max(0.01);
    Duration::from_secs_f64(seconds)
}

pub fn log_path() -> Option<PathBuf> {
    crate::home::user_home().map(|home| home.join(LOG_REL_PATH))
}

pub(super) fn append_log(message: &str) {
    let Some(path) = log_path() else {
        return;
    };
    append_log_at(&path, message);
}

fn append_log_at(path: &Path, message: &str) {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    rotate_log_if_needed(path);
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());
    let message = crate::secret_redaction::redact_for_log(message);
    let _ = writeln!(file, "[{timestamp}] pid={} {message}", std::process::id());
}

/// Roll the hook log over to a single `.1` backup once it reaches
/// [`MAX_LOG_BYTES`], mirroring the daemon event log's single-backup scheme
/// (`daemon::daemon_events`). Best-effort: any error leaves the current log in
/// place and appends continue, so a rotation failure never blocks a tool call.
pub(super) fn rotate_log_if_needed(path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() < MAX_LOG_BYTES {
        return;
    }
    let backup = path.with_extension("log.1");
    let _ = std::fs::remove_file(&backup);
    let _ = std::fs::rename(path, &backup);
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn hook_log_redaction_before_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("block-bad-cmd.log");
        let command = "printf '%s' 'fake-rotation-secret' | gh secret set FAKE";
        append_log_at(&path, command);
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_LOG_BYTES)
            .unwrap();
        append_log_at(&path, command);
        for log in [&path, &path.with_extension("log.1")] {
            let text = std::fs::read_to_string(log).unwrap();
            assert!(!text.contains("fake-rotation-secret"));
            assert!(text.contains("[REDACTED:gh-secret]"));
        }
    }

    #[test]
    fn hook_log_redaction_reported_literal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("block-bad-cmd.log");
        let command = "printf '%s' 'fake-hook-password' | gh secret set FAKE_TEST";
        let payload = parse_payload(
            &format!(
                r#"{{"tool_name":"Bash","tool_input":{{"command":{}}}}}"#,
                serde_json::to_string(command).unwrap()
            ),
            dir.path(),
        )
        .unwrap();
        let original = payload.clone();
        let before = decision_from_payload(&payload, &[]);
        append_log_at(&path, command);
        let logged = std::fs::read_to_string(&path).unwrap();
        assert!(
            !logged.contains("fake-hook-password"),
            "temporary hook log: {logged}"
        );
        assert!(logged.contains("[REDACTED:gh-secret]"));
        let after = decision_from_payload(&payload, &[]);
        assert_eq!(before, after);
        assert_eq!(payload, original);
    }
}

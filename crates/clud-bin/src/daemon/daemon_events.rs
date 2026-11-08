use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use super::paths::daemon_events_path;

const MAX_EVENT_LOG_BYTES: u64 = 1024 * 1024;
const EVENT_LOG_BACKUP_SUFFIX: &str = "1";

struct CachedWriter {
    path: PathBuf,
    file: File,
}

static WRITER: Mutex<Option<CachedWriter>> = Mutex::new(None);

pub fn log_event(
    state_dir: &Path,
    op: &str,
    fields: impl IntoIterator<Item = (&'static str, Value)>,
) {
    let mut event = base_event(op);
    for (key, value) in fields {
        event.insert(key.to_string(), value);
    }
    let _ = append_event_line(&daemon_events_path(state_dir), &Value::Object(event));
}

pub(super) fn request_id() -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_ID.fetch_add(1, Ordering::AcqRel)
}

fn base_event(op: &str) -> Map<String, Value> {
    let mut event = Map::new();
    event.insert("ts_ms".to_string(), json!(unix_millis_now()));
    event.insert("event_id".to_string(), json!(request_id()));
    event.insert("daemon_pid".to_string(), json!(std::process::id()));
    event.insert("thread".to_string(), json!(thread_name()));
    event.insert("op".to_string(), json!(op));
    event
}

fn append_event_line(path: &Path, event: &Value) -> io::Result<()> {
    let mut writer = WRITER
        .lock()
        .map_err(|_| io::Error::other("daemon event log writer lock poisoned"))?;
    if writer.as_ref().is_some_and(|cached| cached.path != path) {
        *writer = None;
    }
    let metadata = fs::metadata(path).ok();
    if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.len() >= MAX_EVENT_LOG_BYTES)
    {
        // Windows cannot rename an open file without FILE_SHARE_DELETE.
        // Drop our cached handle before rotating, then open the fresh file.
        *writer = None;
        rotate_if_needed(path, MAX_EVENT_LOG_BYTES)?;
    } else if metadata.is_none() {
        // A different actor removed the active file; do not keep writing to
        // its unlinked handle.
        *writer = None;
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing daemon event log parent"))?;
    fs::create_dir_all(parent)?;
    // Buffer the entire JSON object + trailing newline into one allocation
    // so we issue a single `write_all` (#378). Without this,
    // `serde_json::to_writer` emits many small `write_all` calls and a
    // concurrent reader doing `read_to_string` can hit EOF mid-object,
    // producing the `EOF while parsing an object` failure the rp_broker /
    // reap-orphans tests saw on Linux ARM and macOS ARM.
    let mut event = event.clone();
    crate::secret_redaction::redact_record_fields(
        &mut event,
        &["error", "write_error", "wait_error"],
    );
    if matches!(
        event.get("op").and_then(Value::as_str),
        Some("adopt_kill_accepted" | "adopt_kill_started" | "adopt_kill_finished")
    ) {
        crate::secret_redaction::redact_record_fields(&mut event, &["reason"]);
    }
    let mut buf = serde_json::to_vec(&event)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    buf.push(b'\n');
    if writer.is_none() {
        *writer = Some(CachedWriter {
            path: path.to_path_buf(),
            file: OpenOptions::new().create(true).append(true).open(path)?,
        });
    }
    let file = &mut writer
        .as_mut()
        .expect("cached event writer just opened")
        .file;
    file.write_all(&buf)?;
    file.flush()
}

/// Roll `path` to its single `.1` backup once it reaches `cap` bytes.
pub(super) fn rotate_if_needed(path: &Path, cap: u64) -> io::Result<()> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    if metadata.len() < cap {
        return Ok(());
    }
    let backup = rotated_path(path);
    if backup.exists() {
        let _ = fs::remove_file(&backup);
    }
    fs::rename(path, backup)
}

fn rotated_path(path: &Path) -> PathBuf {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => path.with_extension(format!("{ext}.{EVENT_LOG_BACKUP_SUFFIX}")),
        None => path.with_extension(EVENT_LOG_BACKUP_SUFFIX),
    }
}

fn unix_millis_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn thread_name() -> String {
    std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_redaction_preserves_daemon_behavior() {
        let tmp = tempfile::tempdir().unwrap();
        let path = daemon_events_path(tmp.path());
        let secret = format!("ghp_{}", "A".repeat(36));
        let event = json!({"op":"adopt_kill_accepted", "request_id":secret, "daemon_pid":42,
            "ts_ms":123, "path":format!("/{secret}"), "error":secret,
            "write_error":secret, "wait_error":secret, "reason":secret});
        append_event_line(&path, &event).unwrap();
        let text = fs::read_to_string(path).unwrap();
        assert_eq!(text.lines().count(), 1);
        let mut actual: Value = serde_json::from_str(text.trim()).unwrap();
        let mut expected = event.clone();
        for field in ["error", "write_error", "wait_error", "reason"] {
            assert!(
                !actual[field].to_string().contains(&secret),
                "{field} persisted verbatim: {}",
                actual[field]
            );
            actual[field] = Value::Null;
            expected[field] = Value::Null;
        }
        assert_eq!(actual, expected);
        assert_eq!(event["error"], secret);
        let ordinary = json!({"op":"request_replied", "error":"ordinary error",
            "write_error":"ordinary write error", "wait_error":"ordinary wait error",
            "reason":"fixed_reason", "request_id":7});
        append_event_line(&daemon_events_path(tmp.path()), &ordinary).unwrap();
        let text = fs::read_to_string(daemon_events_path(tmp.path())).unwrap();
        let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last, ordinary);
    }

    fn read_lines(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn appends_one_json_object_per_line() {
        let tmp = tempfile::tempdir().unwrap();
        log_event(tmp.path(), "daemon_start", [("port", json!(1234))]);
        log_event(
            tmp.path(),
            "adopt_kill_accepted",
            [("pids", json!([1, 2])), ("reason", json!("ctrl_c"))],
        );

        let lines = read_lines(&daemon_events_path(tmp.path()));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["op"], "daemon_start");
        assert_eq!(lines[0]["port"], 1234);
        assert_eq!(lines[1]["op"], "adopt_kill_accepted");
        assert_eq!(lines[1]["pids"], json!([1, 2]));
        assert!(lines[1]["ts_ms"].is_u64());
        assert!(lines[1]["event_id"].is_u64());
    }

    #[test]
    fn cached_handle_survives_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = daemon_events_path(tmp.path());
        fs::write(&path, vec![b'x'; (MAX_EVENT_LOG_BYTES + 1) as usize]).unwrap();

        log_event(tmp.path(), "after_rotate", []);

        let backup = rotated_path(&path);
        assert!(backup.exists(), "expected rotated backup at {backup:?}");
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["op"], "after_rotate");

        log_event(tmp.path(), "after_rotate_again", []);
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["op"], "after_rotate_again");
    }

    // NB: the reader-vs-writer atomicity intent originally captured by a
    // `concurrent_reader_never_sees_partial_jsonl_object` test was removed
    // post-#379 once it became clear that Linux's `O_APPEND` guarantees
    // writer-vs-writer atomicity (covered by the test below) but not
    // reader-vs-writer atomicity. The single-`write_all` fix in #378 is
    // best-effort for readers: it shrinks the partial-read window to a
    // single syscall, which is enough for the production-pattern tests
    // (`daemon::server::tests::reap_orphans_request_writes_jsonl_events`,
    // `adopt_kill_request_writes_jsonl_events`) that exercise the actual
    // daemon access pattern, but a stress test reading in a tight loop on
    // Linux ARM can still observe the race. Those production-pattern tests
    // continue to serve as the regression check.

    #[test]
    fn concurrent_append_keeps_one_valid_json_object_per_line() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().to_path_buf();
        let threads = 12;
        let per_thread = 75;

        std::thread::scope(|scope| {
            for worker in 0..threads {
                let state_dir = state_dir.clone();
                scope.spawn(move || {
                    for seq in 0..per_thread {
                        log_event(
                            &state_dir,
                            "stress",
                            [
                                ("worker", json!(worker)),
                                ("seq", json!(seq)),
                                ("payload", json!("x".repeat(1024))),
                            ],
                        );
                    }
                });
            }
        });

        let text = fs::read_to_string(daemon_events_path(tmp.path())).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), threads * per_thread);
        for (idx, line) in lines.iter().enumerate() {
            let value: Value = serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("invalid jsonl line {idx}: {err}: {line:?}"));
            assert_eq!(value["op"], "stress");
            assert!(value["worker"].is_u64());
            assert!(value["seq"].is_u64());
        }
    }
}

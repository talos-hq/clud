//! Issue #1486: telemetry for the in-session `git` / `gh` pass-through
//! aliases. One JSON line per invocation, appended to
//! `<state>/logs/shim/git-gh.jsonl` (`~/.clud/state/...`, or
//! `$CLUD_DAEMON_STATE_DIR`).
//!
//! Telemetry never changes behaviour: every error is ignored, nothing is
//! written to stdout or stderr, and there is no network and no extra process.
//! The record holds the tool, argv, cwd, exit code, duration, the clud
//! session id and the parent process name (Linux only, from `/proc`); never
//! an environment value. `~/.clud/state` has no sweeper (#1014), so the file
//! rotates to `git-gh.jsonl.1` once it passes [`MAX_BYTES`], keeping at most
//! two files. See `docs/architecture/git-gh-telemetry-shim.md`.

use std::cell::Cell;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Size at which the log rotates. Two files cap the footprint at ~2x this.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;
/// File name under `<state>/logs/shim/`.
pub const FILE_NAME: &str = "git-gh.jsonl";

/// Env vars that carry the clud session id, in preference order. Only these
/// values are read, and only to fill `session_id`.
const SESSION_ID_KEYS: &[&str] = &["CLUD_SESSION_ID", crate::grind_facts::SESSION_ENV];

/// One in-flight invocation. [`Recorder::record`] writes its line once;
/// later calls are no-ops, so a handler can record before it re-raises a
/// signal and the dispatcher's own call does nothing.
pub struct Recorder {
    tool: String,
    argv: Vec<String>,
    started: Instant,
    ts_ms: u64,
    done: Cell<bool>,
    path: Option<PathBuf>,
}

impl Recorder {
    pub fn start(tool: &str, args: &[OsString]) -> Self {
        let path = crate::daemon::default_state_dir()
            .ok()
            .map(|state| log_path(&state));
        Self::start_at(tool, args, path)
    }

    /// [`Recorder::start`] writing to an explicit file, for tests.
    pub fn start_at(tool: &str, args: &[OsString], path: Option<PathBuf>) -> Self {
        Recorder {
            tool: tool.to_string(),
            argv: args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            started: Instant::now(),
            ts_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            done: Cell::new(false),
            path,
        }
    }

    /// Append the line for `exit_code`, once. Never fails, never prints.
    pub fn record(&self, exit_code: i32) {
        if self.done.replace(true) {
            return;
        }
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let record = serde_json::json!({
            "ts_ms": self.ts_ms,
            "tool": self.tool,
            "argv": self.argv,
            "cwd": std::env::current_dir().ok(),
            "exit_code": exit_code,
            "duration_ms": self.started.elapsed().as_millis() as u64,
            "session_id": session_id(),
            "pid": std::process::id(),
            "parent": parent_name(),
        });
        append(path, &record);
    }
}

/// `<state>/logs/shim/git-gh.jsonl`.
pub fn log_path(state: &Path) -> PathBuf {
    state.join("logs").join("shim").join(FILE_NAME)
}

fn session_id() -> Option<String> {
    SESSION_ID_KEYS
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
}

/// The parent's process name when it costs one small file read (Linux
/// `/proc/<ppid>/comm`); `None` elsewhere.
fn parent_name() -> Option<String> {
    crate::platform::shim_telemetry::parent_name()
}

/// Rotate past [`MAX_BYTES`], then append one line. Every error is ignored.
fn append(path: &Path, record: &serde_json::Value) {
    append_with_cap(path, record, MAX_BYTES);
}

fn append_with_cap(path: &Path, record: &serde_json::Value, cap: u64) {
    let mut record = record.clone();
    crate::secret_redaction::redact_record_fields(&mut record, &["argv"]);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() >= cap) {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let _ = std::fs::rename(path, PathBuf::from(rotated));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        // One write per line, so concurrent shims do not interleave lines.
        let _ = file.write_all(format!("{record}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_redaction_preserves_shim_behavior() {
        let tmp = tempfile::tempdir().unwrap();
        let path = log_path(tmp.path());
        let secret = format!("ghp_{}", "A".repeat(36));
        let args = vec![OsString::from("status"), OsString::from(&secret)];
        let recorder = Recorder::start_at("git", &args, Some(path.clone()));
        recorder.record(7);
        recorder.record(9);
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text.lines().count(), 1);
        let mut actual: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        let mut expected = serde_json::json!({
            "ts_ms": recorder.ts_ms, "tool": "git", "argv": ["status", secret],
            "cwd": std::env::current_dir().ok(), "exit_code": 7,
            "duration_ms": actual["duration_ms"], "session_id": session_id(),
            "pid": sysinfo::get_current_pid().unwrap().as_u32(), "parent": parent_name(),
        });
        assert_eq!(recorder.argv[1], secret);
        assert_eq!(args[1], OsString::from(&secret));
        assert!(
            !text.contains(&secret),
            "credential persisted verbatim: {text}"
        );
        assert_eq!(actual["argv"][0], "status");
        actual["argv"] = serde_json::Value::Null;
        expected["argv"] = serde_json::Value::Null;
        assert_eq!(actual, expected);
    }

    fn read_lines(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn one_record_with_every_field_and_no_environment() {
        let dir = tempfile::tempdir().unwrap();
        let path = log_path(dir.path());
        let args = ["clone", "https://github.com/zackees/clud", "x"].map(OsString::from);
        let recorder = Recorder::start_at("git", &args, Some(path.clone()));
        recorder.record(7);
        recorder.record(0);
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1, "one record per invocation");
        let line = &lines[0];
        assert_eq!(line["tool"], "git");
        assert_eq!(
            line["argv"],
            serde_json::json!(["clone", "https://github.com/zackees/clud", "x"])
        );
        assert_eq!(line["exit_code"], 7, "the first recorded exit wins");
        assert!(line["ts_ms"].as_u64().unwrap() > 0);
        assert!(line["duration_ms"].is_u64());
        assert!(line["cwd"].is_string());
        assert!(line.get("session_id").is_some());
        assert!(line.get("parent").is_some());
        let keys: Vec<&String> = line.as_object().unwrap().keys().collect();
        assert!(
            !keys.iter().any(|k| k.contains("env")),
            "never log environment values: {keys:?}"
        );
    }

    #[test]
    fn an_unwritable_path_is_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("state");
        std::fs::write(&blocker, b"a file, not a directory").unwrap();
        let recorder = Recorder::start_at("gh", &[], Some(log_path(&blocker)));
        recorder.record(0);
        Recorder::start_at("gh", &[], None).record(1);
        assert_eq!(std::fs::read(&blocker).unwrap(), b"a file, not a directory");
    }

    #[test]
    fn the_log_rotates_past_the_cap_and_keeps_two_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = log_path(dir.path());
        let record = serde_json::json!({"tool": "git"});
        for _ in 0..3 {
            append_with_cap(&path, &record, 20);
        }
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        assert!(PathBuf::from(rotated).is_file());
        assert!(std::fs::metadata(&path).unwrap().len() < 40);
    }
}

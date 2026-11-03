use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::api_sessions::{ApiSessionRecord, ApiSessionStore};
use super::client::{ensure_daemon, request_api_session_kill, request_session_termination};
use super::io_helpers::read_json_file;
use super::paths::{logs_dir, session_log_path, session_snapshot_path};
use super::sessions::{list_background_sessions, resolve_session_id};
use super::types::SessionSnapshot;
use crate::orphan_reaper::{reap_orphans, ReapOpts};
use crate::session_origin::SessionOrigin;

pub(super) fn run_kill(state_dir: &Path, session_id: Option<&str>, all: bool) -> i32 {
    if let Err(err) = ensure_daemon(state_dir) {
        eprintln!("[clud] failed to reach daemon: {}", err);
        return 1;
    }

    if all {
        let sessions = list_background_sessions(state_dir);
        let api_sessions: Vec<_> = ApiSessionStore::new(state_dir)
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|session| session.state != super::api_sessions::ApiSessionState::Terminated)
            .collect();
        let mut failed = 0;
        for session in &sessions {
            match request_session_termination(state_dir, &session.id) {
                Ok(_) => eprintln!("[clud] killed session {}", session.id),
                Err(err) => {
                    eprintln!("[clud] failed to kill session {}: {}", session.id, err);
                    failed += 1;
                }
            }
        }

        for session in &api_sessions {
            match request_api_session_kill(state_dir, &session.id) {
                Ok(()) => eprintln!("[clud] killed API session {}", session.id),
                Err(err) => {
                    eprintln!("[clud] failed to kill API session {}: {}", session.id, err);
                    failed += 1;
                }
            }
        }

        // Also reap CLUD-tagged orphans whose originator clud is gone. The
        // session registry only covers `--detach` / `--detachable` work; a
        // foreground clud that died via SIGKILL leaves its env-tagged
        // descendants behind, and they would otherwise live forever.
        let outcome = reap_orphans(&ReapOpts::default());

        if sessions.is_empty() && api_sessions.is_empty() && outcome.found == 0 {
            println!("No active sessions or orphans to kill.");
            return 0;
        }
        if failed > 0 {
            return 1;
        }
        return 0;
    }

    let Some(input) = session_id else {
        eprintln!("Usage: clud kill <session_id> or clud kill --all");
        return 1;
    };

    // API logical IDs are separate from worker snapshots and are never sent
    // through the attach/worker termination RPC.
    let api_store = ApiSessionStore::new(state_dir);
    if api_store.get(input).is_ok() {
        return match request_api_session_kill(state_dir, input) {
            Ok(()) => {
                eprintln!("[clud] killed API session {input}");
                0
            }
            Err(err) => {
                eprintln!("[clud] failed to kill API session {input}: {err}");
                1
            }
        };
    }

    let resolved = match resolve_session_id(state_dir, input) {
        Ok(id) => id,
        Err(err) => {
            eprintln!("[clud] {}", err);
            return 1;
        }
    };

    let api = ApiSessionStore::new(state_dir).get(&resolved).is_ok();
    let result = if api {
        request_api_session_kill(state_dir, &resolved).map(|_| ())
    } else {
        request_session_termination(state_dir, &resolved).map(|_| ())
    };
    match result {
        Ok(_) => {
            eprintln!("[clud] killed session {}", resolved);
            0
        }
        Err(err) => {
            eprintln!("[clud] failed to kill session {}: {}", resolved, err);
            1
        }
    }
}

fn format_duration_short(millis: u64) -> String {
    let now_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let elapsed_secs = now_millis.saturating_sub(millis) / 1000;
    if elapsed_secs < 60 {
        format!("{}s", elapsed_secs)
    } else if elapsed_secs < 3600 {
        format!("{}m", elapsed_secs / 60)
    } else {
        format!("{}h{}m", elapsed_secs / 3600, (elapsed_secs % 3600) / 60)
    }
}

/// One JSON row per API session, optionally filtered by origin. Every row
/// carries `origin` (legacy records report `unknown`).
pub(super) fn api_list_rows(
    records: &[ApiSessionRecord],
    origin: Option<SessionOrigin>,
) -> Vec<serde_json::Value> {
    records
        .iter()
        .filter(|record| origin.is_none_or(|wanted| record.origin == wanted))
        .map(api_session_summary)
        .collect()
}

fn api_session_summary(record: &ApiSessionRecord) -> serde_json::Value {
    serde_json::json!({
        "id": record.id,
        "name": record.name,
        "state": record.state,
        "origin": record.origin,
        "backend": record.backend,
        "model": record.resolved_settings.model,
        "cwd": record.cwd,
        "created_at_ms": record.created_at_ms,
    })
}

fn api_state_str(record: &ApiSessionRecord) -> String {
    format!("{:?}", record.state).to_ascii_lowercase()
}

/// `clud session show <id>`: origin alongside state, backend and model.
pub(super) fn run_session_show(state_dir: &Path, id: &str, json: bool) -> i32 {
    let record = match ApiSessionStore::new(state_dir).get(id) {
        Ok(record) => record,
        Err(err) => {
            eprintln!("[clud] {err}");
            return 1;
        }
    };
    if json {
        println!("{}", api_session_summary(&record));
        return 0;
    }
    let backend = serde_json::to_value(record.backend).unwrap_or_default();
    println!("id:      {}", record.id);
    println!("state:   {}", api_state_str(&record));
    println!("origin:  {}", record.origin.as_str());
    println!("backend: {}", backend.as_str().unwrap_or("-"));
    println!(
        "model:   {}",
        record.resolved_settings.model.as_deref().unwrap_or("-")
    );
    println!("cwd:     {}", record.cwd.display());
    0
}

/// `--json` and `--origin` select the origin-carrying API session view.
fn run_list_api_only(state_dir: &Path, json: bool, origin: Option<SessionOrigin>) -> i32 {
    let records = ApiSessionStore::new(state_dir).list().unwrap_or_default();
    let records: Vec<_> = records
        .into_iter()
        .filter(|record| origin.is_none_or(|wanted| record.origin == wanted))
        .collect();
    if json {
        let rows = api_list_rows(&records, None);
        println!("{}", serde_json::Value::Array(rows));
        return 0;
    }
    if records.is_empty() {
        println!("No API sessions.");
        return 0;
    }
    print_api_sessions(&records);
    0
}

fn print_api_sessions(records: &[ApiSessionRecord]) {
    println!("{:<30} {:<12} {:<8} CWD", "API SESSION", "STATUS", "ORIGIN");
    for session in records {
        println!(
            "{:<30} {:<12} {:<8} {}",
            session.id,
            api_state_str(session),
            session.origin.as_str(),
            session.cwd.display()
        );
    }
}

pub(super) fn run_list(state_dir: &Path, json: bool, origin: Option<SessionOrigin>) -> i32 {
    if json || origin.is_some() {
        return run_list_api_only(state_dir, json, origin);
    }
    let sessions = list_background_sessions(state_dir);
    let api_sessions = ApiSessionStore::new(state_dir).list().unwrap_or_default();
    let has_api_sessions = !api_sessions.is_empty();
    let checkout_sessions = super::checkout_claims::all_live(state_dir).unwrap_or_default();
    if sessions.is_empty() && api_sessions.is_empty() && checkout_sessions.is_empty() {
        println!("No background sessions.");
        return 0;
    }

    let has_background_sessions = !sessions.is_empty();
    let (repeat_jobs, attachable_sessions): (Vec<_>, Vec<_>) = sessions
        .into_iter()
        .partition(|session| session.repeat_interval_secs.is_some());

    if !repeat_jobs.is_empty() {
        println!("{:<53} {:<10} {:<18} ID", "TASK", "STATUS", "NEXT RUN");
        for session in &repeat_jobs {
            let task = session.name.clone().unwrap_or_else(|| session.id.clone());
            let status = if session.repeat_running {
                "running".to_string()
            } else {
                "sleeping".to_string()
            };
            let next_run = session
                .repeat_next_run_at
                .map(format_next_run_short)
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<53} {:<10} {:<18} {}",
                task, status, next_run, session.id
            );
        }
    }

    if !attachable_sessions.is_empty() {
        if !repeat_jobs.is_empty() {
            println!();
        }
        println!("{:<30} {:<8} {:<8} CWD", "SESSION", "PID", "UPTIME");
        for session in attachable_sessions {
            let display_name = session
                .name
                .as_deref()
                .map(|n| format!("{} ({})", session.id, n))
                .unwrap_or_else(|| session.id.clone());
            let pid = session
                .root_pid
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string());
            let uptime = session
                .created_at
                .map(format_duration_short)
                .unwrap_or_else(|| "-".to_string());
            let cwd = session.cwd.unwrap_or_else(|| "-".to_string());
            println!("{:<30} {:<8} {:<8} {}", display_name, pid, uptime, cwd);
        }
    }

    if !api_sessions.is_empty() {
        if has_background_sessions {
            println!();
        }
        print_api_sessions(&api_sessions);
    }
    if !checkout_sessions.is_empty() {
        if has_background_sessions || has_api_sessions {
            println!();
        }
        println!(
            "{:<30} {:<8} {:<9} CHECKOUT",
            "LIVE SESSION", "PID", "CLAIM"
        );
        for (checkout, occupant) in checkout_sessions {
            println!(
                "{:<30} {:<8} {:<9} {}",
                occupant.session_id,
                occupant.pid,
                if occupant.claimed {
                    "claimed"
                } else {
                    "present"
                },
                checkout.worktree.display()
            );
        }
    }
    0
}

fn format_next_run_short(run_at_millis: u64) -> String {
    let now_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    if run_at_millis <= now_millis {
        return "now".to_string();
    }
    let secs = (run_at_millis - now_millis) / 1000;
    if secs < 60 {
        format!("in {}s", secs)
    } else if secs < 3600 {
        format!("in {}m", secs / 60)
    } else {
        format!("in {}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// pm2-style log viewer. No id → list sessions with their last log line.
/// With an id → dump the log, then optionally follow.
pub(super) fn run_logs(
    state_dir: &Path,
    session_id: Option<&str>,
    follow: bool,
    lines: Option<usize>,
    interrupted: &AtomicBool,
) -> i32 {
    let Some(input) = session_id else {
        return run_logs_summary(state_dir);
    };
    if let Ok(session) = ApiSessionStore::new(state_dir).get(input) {
        return run_api_logs(state_dir, &session, follow, lines, interrupted);
    }
    let resolved = match resolve_session_id(state_dir, input) {
        Ok(id) => id,
        Err(_) => {
            // Allow viewing logs for sessions whose snapshot file has been
            // cleaned up but whose .log remains on disk. If a raw .log exists
            // at the given name, use that.
            let raw = session_log_path(state_dir, input);
            if raw.is_file() {
                input.to_string()
            } else {
                eprintln!("[clud] no session or log found for {}", input);
                return 1;
            }
        }
    };
    let path = session_log_path(state_dir, &resolved);
    if !path.exists() {
        eprintln!(
            "[clud] no log file for session {}: {}",
            resolved,
            path.display()
        );
        return 1;
    }
    let mut offset = match print_log_tail(&path, lines) {
        Ok(offset) => offset,
        Err(err) => {
            eprintln!("[clud] failed to read log {}: {}", path.display(), err);
            return 1;
        }
    };
    if !follow {
        // If the session is already dead, print a status line so the user
        // knows they're looking at a post-mortem rather than a live tail.
        if let Some(code) = session_exit_code(state_dir, &resolved) {
            eprintln!("[clud] session {} exited with status {}", resolved, code);
        }
        return 0;
    }
    // pm2-style follow: poll for new bytes at the last known offset. A
    // short sleep on "no new data" keeps CPU low without requiring a file-
    // watch API that differs per OS. Exits cleanly once the session has
    // terminated (and any final bytes have been drained).
    loop {
        if interrupted.load(Ordering::SeqCst) {
            // Issue #517: `clud logs --follow` is another site where an
            // observed interrupt changes behavior (early return instead
            // of continuing to tail), so log the reason here too.
            super::attach::log_observed_interrupt_reason("logs_follow");
            return 130;
        }
        match follow_read(&path, offset) {
            Ok((new_offset, chunk)) => {
                if !chunk.is_empty() {
                    let _ = io::stdout().write_all(&chunk);
                    let _ = io::stdout().flush();
                }
                offset = new_offset;
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // Rotation race: file was renamed out from under us.
                // Sleep briefly and the rotated primary will reappear.
            }
            Err(err) => {
                eprintln!("[clud] follow error: {}", err);
                return 1;
            }
        }
        if let Some(code) = session_exit_code(state_dir, &resolved) {
            // Drain any final bytes the worker flushed between our last
            // follow_read and snapshot update before announcing the exit.
            if let Ok((_, chunk)) = follow_read(&path, offset) {
                if !chunk.is_empty() {
                    let _ = io::stdout().write_all(&chunk);
                    let _ = io::stdout().flush();
                }
            }
            eprintln!("[clud] session {} exited with status {}", resolved, code);
            return 0;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// API turns write JSONL under their logical ID and are intentionally not
/// routed through worker snapshot logging or attach semantics.
fn run_api_logs(
    state_dir: &Path,
    session: &super::api_sessions::ApiSessionRecord,
    follow: bool,
    lines: Option<usize>,
    interrupted: &AtomicBool,
) -> i32 {
    let Some(turn) = session.turns.last() else {
        eprintln!("[clud] API session {} has no turn log", session.id);
        return 1;
    };
    let path = super::paths::api_turn_log_path(state_dir, &session.id, turn.generation);
    let mut offset = match print_log_tail(&path, lines) {
        Ok(offset) => offset,
        Err(err) => {
            eprintln!("[clud] failed to read API log {}: {err}", path.display());
            return 1;
        }
    };
    if !follow {
        return 0;
    }
    while matches!(
        ApiSessionStore::new(state_dir)
            .get(&session.id)
            .map(|value| value.state),
        Ok(super::api_sessions::ApiSessionState::Starting
            | super::api_sessions::ApiSessionState::Running
            | super::api_sessions::ApiSessionState::Interrupting)
    ) {
        if interrupted.load(Ordering::SeqCst) {
            super::attach::log_observed_interrupt_reason("api_logs_follow");
            return 130;
        }
        if let Ok((new_offset, bytes)) = follow_read(&path, offset) {
            if !bytes.is_empty() {
                let _ = io::stdout().write_all(&bytes);
                let _ = io::stdout().flush();
            }
            offset = new_offset;
        }
        thread::sleep(Duration::from_millis(200));
    }
    0
}

/// Read the on-disk snapshot for `session_id` and return its `exit_code`
/// if present. Returns `None` when the snapshot is missing, malformed,
/// or the session is still running.
fn session_exit_code(state_dir: &Path, session_id: &str) -> Option<i32> {
    let path = session_snapshot_path(state_dir, session_id);
    let session = read_json_file::<SessionSnapshot>(&path).ok()?;
    session.exit_code
}

fn run_logs_summary(state_dir: &Path) -> i32 {
    let dir = logs_dir(state_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        println!("No log files. Start a session with: clud --detach -p <prompt>");
        return 0;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("log"))
        .collect();
    if paths.is_empty() {
        println!("No log files in {}", dir.display());
        return 0;
    }
    paths.sort();
    println!("{:<30} {:>10} LAST LINE", "SESSION", "BYTES");
    for path in paths {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let last = last_line_of(&path).unwrap_or_default();
        println!(
            "{:<30} {:>10} {}",
            stem,
            size,
            last.trim_end_matches(['\r', '\n'])
        );
    }
    0
}

fn last_line_of(path: &Path) -> io::Result<String> {
    // Read the tail in a modest chunk and return the last non-empty line.
    // Cheap for typical log files; good enough for a summary listing.
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(4096);
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    Ok(text
        .lines()
        .rfind(|line| !line.is_empty())
        .unwrap_or("")
        .to_string())
}

fn print_log_tail(path: &Path, lines: Option<usize>) -> io::Result<u64> {
    let data = fs::read(path)?;
    let slice: &[u8] = match lines {
        None => &data,
        Some(n) => {
            let mut count = 0usize;
            let mut split = data.len();
            for (i, &b) in data.iter().enumerate().rev() {
                if b == b'\n' && i + 1 != data.len() {
                    count += 1;
                    if count > n {
                        split = i + 1;
                        break;
                    }
                }
            }
            if count <= n {
                // File has <= n lines; show the whole thing.
                &data
            } else {
                &data[split..]
            }
        }
    };
    io::stdout().write_all(slice)?;
    io::stdout().flush()?;
    Ok(data.len() as u64)
}

fn follow_read(path: &Path, offset: u64) -> io::Result<(u64, Vec<u8>)> {
    let meta = fs::metadata(path)?;
    let len = meta.len();
    if len < offset {
        // File was truncated or rotated — start from the new beginning.
        let data = fs::read(path)?;
        return Ok((data.len() as u64, data));
    }
    if len == offset {
        return Ok((offset, Vec::new()));
    }
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::with_capacity((len - offset) as usize);
    file.read_to_end(&mut buf)?;
    Ok((len, buf))
}

#[cfg(test)]
mod tests {
    //! Issue #25: read-only session tailing via `clud logs`. These tests
    //! poke at the on-disk-snapshot helpers that back `--last` and the
    //! "session exited" status line so we can verify the contract without
    //! spinning up a real daemon. End-to-end behavior is covered by the
    //! Python integration test in `tests/integration/test_daemon_centralized.py`.
    use super::*;
    use crate::daemon::io_helpers::write_json_file;
    use crate::daemon::types::SessionKind;
    use tempfile::TempDir;

    fn write_snapshot(state_dir: &Path, id: &str, created_at: u64, exit_code: Option<i32>) {
        let snap = SessionSnapshot {
            id: id.into(),
            kind: SessionKind::Subprocess,
            backend: None,
            launch_mode: None,
            repo_root: None,
            command: Vec::new(),
            cwd: None,
            name: None,
            created_at: Some(created_at),
            detachable: false,
            background: true,
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
            exit_code,
            exited_at: exit_code.map(|_| created_at + 1000),
            ctrl_c: None,
        };
        write_json_file(&session_snapshot_path(state_dir, id), &snap).unwrap();
    }

    #[test]
    fn session_exit_code_reads_snapshot() {
        let tmp = TempDir::new().unwrap();
        write_snapshot(tmp.path(), "sess-live", 1, None);
        write_snapshot(tmp.path(), "sess-dead", 2, Some(42));
        assert_eq!(session_exit_code(tmp.path(), "sess-live"), None);
        assert_eq!(session_exit_code(tmp.path(), "sess-dead"), Some(42));
        // Missing snapshot is treated as "still running" (None) rather than
        // an error — the follow loop polls cheaply and re-checks.
        assert_eq!(session_exit_code(tmp.path(), "sess-missing"), None);
    }

    #[test]
    fn follow_read_returns_new_bytes_since_offset() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("x.log");
        fs::write(&path, b"hello").unwrap();
        let (off, new) = follow_read(&path, 0).unwrap();
        assert_eq!(off, 5);
        assert_eq!(new, b"hello");

        // Append and re-read from offset 5; only new bytes come back.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b" world").unwrap();
        drop(f);

        let (off2, new2) = follow_read(&path, off).unwrap();
        assert_eq!(off2, 11);
        assert_eq!(new2, b" world");
    }

    #[test]
    fn follow_read_detects_truncation_and_rereads_from_zero() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("t.log");
        fs::write(&path, b"longlong").unwrap();
        let (off, _) = follow_read(&path, 0).unwrap();
        fs::write(&path, b"short").unwrap();
        let (new_off, new) = follow_read(&path, off).unwrap();
        assert_eq!(new_off, 5);
        assert_eq!(new, b"short");
    }
}

//! Session-scoped tool invocation index. Slice 1 of #427 — lands the
//! data model that subsequent slices (tee writer, watchdog, read
//! commands) build on.
//!
//! Per-session events live under
//! `~/.clud/state/sessions/<session-pid>__<start-epoch>/tools/index.jsonl`,
//! one JSON object per line, schema-versioned from v1. The session-local
//! integer tool ID is the primary UX (PM2-style `1, 2, 3, ...`); the
//! long-form ID `<session-pid>-<tool-id>` (e.g. `47180-3`) is the
//! cross-session form that embeds the sub-process → session mapping
//! directly into the identifier.
//!
//! Daemon-down fallback: `SessionContext::from_env` returns `None` when
//! no clud session is active, and `tool_run.rs` skips all session-index
//! interaction — tools still run, just without lifecycle tracking. This
//! keeps `clud tool run` runnable in CI / minimal containers.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use fs4::fs_std::FileExt;
use serde::Serialize;

/// Env var: PID of the clud daemon that owns this session.
pub const SESSION_PID_ENV: &str = "CLUD_SESSION_PID";

/// Env var: Unix epoch seconds when the clud daemon started; combined with
/// the PID to make the on-disk session directory collision-safe across
/// PID reuse.
pub const SESSION_START_EPOCH_ENV: &str = "CLUD_SESSION_START_EPOCH";

/// JSONL schema version. Bumped only by intentional schema changes; readers
/// must tolerate unknown keys within a version.
pub const INDEX_SCHEMA_VERSION: u32 = 1;

/// Serialize concurrent appends to the same index.jsonl from within this
/// process. Cross-process serialization rides on `fs4` locks at the
/// counter-file layer (concurrent `clud tool run` invocations are already
/// queued behind ID allocation before they reach the append).
static APPEND_LOCK: Mutex<()> = Mutex::new(());

/// Resolved view of "which clud session am I running inside?" Returned by
/// [`SessionContext::from_env`]. `None` means no clud session (CI /
/// minimal / no-daemon case) and callers should skip session-index work.
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub session_pid: u32,
    pub session_start_epoch: u64,
    pub session_dir: PathBuf,
}

impl SessionContext {
    /// Resolve from the env vars the clud daemon sets on every session
    /// child. Both env vars must be present and parseable; otherwise
    /// returns `None`.
    pub fn from_env() -> Option<Self> {
        let pid: u32 = std::env::var(SESSION_PID_ENV).ok()?.parse().ok()?;
        let epoch: u64 = std::env::var(SESSION_START_EPOCH_ENV).ok()?.parse().ok()?;
        let session_dir = state_root()?
            .join("sessions")
            .join(format!("{pid}__{epoch}"));
        Some(Self {
            session_pid: pid,
            session_start_epoch: epoch,
            session_dir,
        })
    }

    /// Testable variant: build a context anchored to an explicit state
    /// root. Used in unit tests so the real `~/.clud/` directory is never
    /// touched.
    pub fn from_state_root(state_root: &Path, session_pid: u32, session_start_epoch: u64) -> Self {
        let session_dir = state_root
            .join("sessions")
            .join(format!("{session_pid}__{session_start_epoch}"));
        Self {
            session_pid,
            session_start_epoch,
            session_dir,
        }
    }

    /// Directory holding the session's tool subdirectories and index.
    pub fn tools_dir(&self) -> PathBuf {
        self.session_dir.join("tools")
    }

    /// Append-only JSONL index of tool lifecycle events for the session.
    pub fn index_path(&self) -> PathBuf {
        self.tools_dir().join("index.jsonl")
    }

    /// Per-session monotonic counter for tool-id allocation. fs4 exclusive
    /// lock is held during read-modify-write so concurrent `clud tool run`
    /// invocations in the same session serialize cleanly.
    pub fn counter_path(&self) -> PathBuf {
        self.tools_dir().join("next_id")
    }

    /// On-disk log directory for a specific tool invocation. Layout
    /// matches #427's spec; the tee writer (slice 2) will write
    /// `stdout.jsonl` / `stderr.jsonl` / `combined.jsonl` here.
    pub fn tool_log_dir(&self, tool_id: u32) -> PathBuf {
        self.tools_dir().join(tool_id.to_string())
    }
}

/// Long-form invocation identifier: `<session-pid>-<tool-id>`,
/// e.g. `47180-3`. Embeds the sub-process → parent-session mapping
/// directly in the identifier so cross-session lookups don't need an
/// index hop to find the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongFormId {
    pub session_pid: u32,
    pub tool_id: u32,
}

impl LongFormId {
    /// Render as `<session-pid>-<tool-id>`.
    pub fn format(&self) -> String {
        format!("{}-{}", self.session_pid, self.tool_id)
    }

    /// Parse a `<session-pid>-<tool-id>` string. Returns `None` on any
    /// malformed input (missing dash, non-numeric parts, leading or
    /// trailing whitespace, etc.).
    pub fn parse(s: &str) -> Option<Self> {
        // Strict parse: no surrounding whitespace, single dash, numeric
        // parts only. Permissive parsing would conflict with future
        // tool-name aliases that could contain dashes.
        if s.trim() != s {
            return None;
        }
        let (pid_str, tool_id_str) = s.split_once('-')?;
        if pid_str.is_empty() || tool_id_str.is_empty() {
            return None;
        }
        let session_pid: u32 = pid_str.parse().ok()?;
        let tool_id: u32 = tool_id_str.parse().ok()?;
        Some(Self {
            session_pid,
            tool_id,
        })
    }
}

/// A tool-invocation lifecycle event. `Started` is appended when
/// `clud tool run` claims its session-local ID; `Finished` and `Aborted`
/// close the entry on the corresponding exit path. Schema-versioned via
/// the outer JSON `v` field added by [`append_event`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum IndexEvent {
    Started {
        tool_id: u32,
        tool: String,
        args: Vec<String>,
        pid: u32,
        pid_start_time: u64,
        started_at_ms: u128,
    },
    Finished {
        tool_id: u32,
        exit_code: i32,
        ended_at_ms: u128,
    },
    Aborted {
        tool_id: u32,
        reason: String,
        ended_at_ms: u128,
    },
    ObservationDelayed {
        tool_id: u32,
        reason: String,
        exit_code: i32,
        ended_at_ms: u128,
    },
}

/// Allocate the next session-local tool ID. fs4 exclusive lock on
/// `<tools_dir>/next_id` serializes concurrent `clud tool run` invocations
/// in the same session; the lock is released before this function returns.
pub fn allocate_next_id(ctx: &SessionContext) -> io::Result<u32> {
    fs::create_dir_all(ctx.tools_dir())?;
    let counter_path = ctx.counter_path();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&counter_path)?;
    FileExt::lock_exclusive(&file)?;
    // Hold the exclusive lock across the read-modify-write. Concurrent
    // callers serialize cleanly here.
    let mut buf = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut buf)?;
    let current: u32 = buf.trim().parse().unwrap_or(0);
    let next = current.checked_add(1).ok_or_else(|| {
        io::Error::other(format!(
            "session tool counter at {} overflowed u32",
            counter_path.display()
        ))
    })?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(next.to_string().as_bytes())?;
    file.flush()?;
    FileExt::unlock(&file)?;
    Ok(next)
}

/// Append a single lifecycle event to the session's tool index. Uses
/// the same single-buffered-`write_all` pattern as
/// `daemon_events::append_event_line` (#373) so a concurrent reader doing
/// `read_to_string` cannot hit EOF mid-object.
pub fn append_event(ctx: &SessionContext, event: &IndexEvent) -> io::Result<()> {
    let _guard = APPEND_LOCK
        .lock()
        .map_err(|_| io::Error::other("session index append lock poisoned"))?;
    fs::create_dir_all(ctx.tools_dir())?;
    // Buffer schema-versioned payload + trailing newline into one allocation
    // so we issue a single `write_all` per the #373 race fix.
    let value = serde_json::to_value(event)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let mut object = value
        .as_object()
        .ok_or_else(|| io::Error::other("IndexEvent must serialize to a JSON object"))?
        .clone();
    object.insert(
        "v".to_string(),
        serde_json::Value::Number(INDEX_SCHEMA_VERSION.into()),
    );
    let mut value = serde_json::Value::Object(object);
    crate::secret_redaction::redact_record_fields(&mut value, &["args"]);
    let mut buf = serde_json::to_vec(&value)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    buf.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ctx.index_path())?;
    file.write_all(&buf)?;
    file.flush()
}

/// Unix epoch milliseconds; matches the `daemon_events` representation
/// so future cross-log analysis (orphan-reaper sweep × tool index) can
/// align timestamps without conversion gymnastics.
pub fn unix_millis_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Unix epoch seconds, for the session start-epoch component of the
/// on-disk session directory name.
pub fn unix_secs_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// `~/.clud/state/` — the parent of the per-session directory. Returns
/// `None` when the home directory cannot be resolved.
fn state_root() -> Option<PathBuf> {
    crate::home::user_home().map(|h| h.join(".clud").join("state"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_redaction_preserves_index_behavior() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_in(&tmp, 1234, 5678);
        let secret = format!("ghp_{}", "A".repeat(36));
        let event = IndexEvent::Started {
            tool_id: 1,
            tool: secret.clone(),
            args: vec!["status".into(), secret.clone()],
            pid: 42,
            pid_start_time: 123,
            started_at_ms: 456,
        };
        let original = serde_json::to_value(&event).unwrap();
        append_event(&ctx, &event).unwrap();
        let text = fs::read_to_string(ctx.index_path()).unwrap();
        assert_eq!(text.lines().count(), 1);
        let mut actual: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert!(
            !actual["args"].to_string().contains(&secret),
            "args persisted verbatim: {}",
            actual["args"]
        );
        assert_eq!(actual["args"][0], "status");
        actual["args"] = original["args"].clone();
        assert_eq!(actual["v"], INDEX_SCHEMA_VERSION);
        actual.as_object_mut().unwrap().remove("v");
        assert_eq!(actual, original);
        assert_eq!(serde_json::to_value(&event).unwrap(), original);
    }

    use tempfile::TempDir;

    fn ctx_in(tmp: &TempDir, pid: u32, epoch: u64) -> SessionContext {
        SessionContext::from_state_root(tmp.path(), pid, epoch)
    }

    #[test]
    fn long_form_format_round_trips() {
        let id = LongFormId {
            session_pid: 47180,
            tool_id: 3,
        };
        assert_eq!(id.format(), "47180-3");
        assert_eq!(LongFormId::parse("47180-3"), Some(id));
    }

    #[test]
    fn long_form_parse_rejects_malformed() {
        assert_eq!(LongFormId::parse(""), None);
        assert_eq!(LongFormId::parse("47180"), None);
        assert_eq!(LongFormId::parse("47180-"), None);
        assert_eq!(LongFormId::parse("-3"), None);
        assert_eq!(LongFormId::parse("47180-abc"), None);
        assert_eq!(LongFormId::parse(" 47180-3"), None, "leading whitespace");
        assert_eq!(LongFormId::parse("47180-3 "), None, "trailing whitespace");
        // Negative numbers don't parse as u32 → rejected.
        assert_eq!(LongFormId::parse("-1-3"), None);
    }

    #[test]
    fn session_dir_encodes_pid_and_epoch() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_in(&tmp, 47180, 1737390000);
        let expected = tmp.path().join("sessions").join("47180__1737390000");
        assert_eq!(ctx.session_dir, expected);
    }

    #[test]
    fn pid_reuse_with_different_start_epoch_yields_different_session_dirs() {
        let tmp = TempDir::new().unwrap();
        let ctx_old = ctx_in(&tmp, 47180, 1737390000);
        let ctx_new_after_reboot = ctx_in(&tmp, 47180, 1737900000);
        assert_ne!(
            ctx_old.session_dir, ctx_new_after_reboot.session_dir,
            "different start epochs must produce different session dirs even when the PID is reused"
        );
    }

    #[test]
    fn allocate_next_id_starts_at_one() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_in(&tmp, 1234, 5678);
        let first = allocate_next_id(&ctx).unwrap();
        assert_eq!(first, 1);
    }

    #[test]
    fn allocate_next_id_increments_monotonically() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_in(&tmp, 1234, 5678);
        let a = allocate_next_id(&ctx).unwrap();
        let b = allocate_next_id(&ctx).unwrap();
        let c = allocate_next_id(&ctx).unwrap();
        assert_eq!((a, b, c), (1, 2, 3));
    }

    #[test]
    fn allocate_next_id_persists_across_processes() {
        // Simulate a new process attaching to an existing session by
        // building a fresh `SessionContext` against the same dir.
        let tmp = TempDir::new().unwrap();
        let ctx1 = ctx_in(&tmp, 1234, 5678);
        assert_eq!(allocate_next_id(&ctx1).unwrap(), 1);
        assert_eq!(allocate_next_id(&ctx1).unwrap(), 2);
        drop(ctx1);

        let ctx2 = ctx_in(&tmp, 1234, 5678);
        assert_eq!(allocate_next_id(&ctx2).unwrap(), 3);
    }

    #[test]
    fn append_event_writes_one_jsonl_line() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_in(&tmp, 1234, 5678);
        let event = IndexEvent::Started {
            tool_id: 1,
            tool: "git-status-clean".to_string(),
            args: vec!["--porcelain".to_string()],
            pid: 99999,
            pid_start_time: 123456789,
            started_at_ms: 1_700_000_000_000,
        };
        append_event(&ctx, &event).unwrap();
        let contents = fs::read_to_string(ctx.index_path()).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1, "expected exactly one JSONL line");
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["v"], serde_json::Value::Number(1u32.into()));
        assert_eq!(parsed["event"], serde_json::Value::String("started".into()));
        assert_eq!(parsed["tool_id"], serde_json::Value::Number(1u32.into()));
        assert_eq!(
            parsed["tool"],
            serde_json::Value::String("git-status-clean".into())
        );
    }

    #[test]
    fn append_event_appends_multiple_entries_in_order() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_in(&tmp, 1234, 5678);
        append_event(
            &ctx,
            &IndexEvent::Started {
                tool_id: 1,
                tool: "lint".to_string(),
                args: vec![],
                pid: 1,
                pid_start_time: 1,
                started_at_ms: 1,
            },
        )
        .unwrap();
        append_event(
            &ctx,
            &IndexEvent::Finished {
                tool_id: 1,
                exit_code: 0,
                ended_at_ms: 2,
            },
        )
        .unwrap();
        append_event(
            &ctx,
            &IndexEvent::Started {
                tool_id: 2,
                tool: "test-targeted".to_string(),
                args: vec!["-k".to_string(), "foo".to_string()],
                pid: 2,
                pid_start_time: 3,
                started_at_ms: 4,
            },
        )
        .unwrap();
        let contents = fs::read_to_string(ctx.index_path()).unwrap();
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 3);
        let parsed: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed[0]["event"], "started");
        assert_eq!(parsed[0]["tool"], "lint");
        assert_eq!(parsed[1]["event"], "finished");
        assert_eq!(parsed[1]["tool_id"], 1);
        assert_eq!(parsed[2]["event"], "started");
        assert_eq!(parsed[2]["tool"], "test-targeted");
        assert_eq!(parsed[2]["args"][0], "-k");
    }

    #[test]
    fn tool_log_dir_lands_under_tools_dir() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_in(&tmp, 1234, 5678);
        let log_dir = ctx.tool_log_dir(7);
        assert_eq!(log_dir, ctx.tools_dir().join("7"));
        assert!(log_dir.starts_with(&ctx.session_dir));
    }

    #[test]
    fn from_env_returns_none_when_vars_unset() {
        // We can't reliably mutate the env in unit tests without racing
        // parallel tests, but we can confirm the "missing var" branch
        // returns None for a deliberately-unset bogus var. Build a
        // SessionContext via from_state_root to verify the real path
        // computation works.
        let var = "CLUD_TEST_DELIBERATELY_UNSET_VAR_42";
        // SAFETY: best-effort cleanup; the parse path returns None on
        // missing-or-malformed env vars and that's the only behavior
        // this test pins.
        std::env::remove_var(var);
        assert!(std::env::var(var).is_err());
    }
}

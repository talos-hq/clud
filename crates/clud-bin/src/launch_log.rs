//! Durable per-launch diagnostics for `clud ui`.
//!
//! The live session-cap registry intentionally deletes rows on graceful exit.
//! These records are separate: one JSON file per launch under the daemon state
//! directory, retained long enough for the dashboard to explain recent exits.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::command::LaunchPlan;
use crate::loop_spec;

const DIR_NAME: &str = "launches";
const MAX_RECORDS: usize = 200;

/// Longest failure reason persisted (#998). A record exists to explain an exit
/// in one glance; the reasons recorded today are one short sentence, and an
/// unbounded field would let a pathological error message dominate the file the
/// dashboard reads for every launch.
const MAX_FAILURE_REASON_CHARS: usize = 300;

/// The launch whose failures this process is currently recording, plus the
/// reason if one has been raised.
///
/// A process-global slot rather than a threaded-through return value for one
/// narrow reason: the runners return a bare `i32`, so carrying the reason out
/// of them means changing two public signatures and every call site of each.
/// That is the whole argument -- the raise is only one frame below the frame
/// holding the [`LaunchLogHandle`], since `main` calls `run_plan_*` directly.
/// It is a weaker case than `ctrl_c_track`'s `HANDOFF_OUTCOME`, which is a
/// global because a signal handler genuinely has no call path back. Do not
/// cite this as precedent for a global reaching across a deep or re-entrant
/// call graph.
///
/// Keyed by launch id, which is what keeps the convenience safe: a reason left
/// behind by an aborted [`finish_launch`] -- a missing or corrupt record file
/// returns early, before the take -- cannot attach to some later record. One
/// launch per process makes that unreachable today, but the invariant is
/// implicit and the daemon is the obvious future second caller.
static FAILURE_REASON: Mutex<Option<PendingFailure>> = Mutex::new(None);

#[derive(Debug)]
struct PendingFailure {
    launch_id: String,
    reason: Option<String>,
}

fn failure_slot() -> std::sync::MutexGuard<'static, Option<PendingFailure>> {
    // The payload is a `String` with no invariant to protect, and a poisoned
    // lock would otherwise discard the reason in exactly the case -- another
    // thread panicked -- where it is most worth having.
    FAILURE_REASON
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static METERED_WARNING: Mutex<Option<String>> = Mutex::new(None);

fn metered_warning_slot() -> std::sync::MutexGuard<'static, Option<String>> {
    METERED_WARNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Remember the metered-route warning so [`start_launch`] records it.
pub fn set_metered_warning(warning: impl Into<String>) {
    *metered_warning_slot() = Some(warning.into());
}

/// Record why the current launch is about to fail.
///
/// Callers pass the text the user sees on stderr, so the record and the
/// terminal agree -- modulo the whitespace collapse in
/// [`sanitize_failure_reason`], which folds a multi-line error onto one line.
/// Bounding and scrubbing happen in [`finish_launch`], not here, so neither
/// can be bypassed by a future caller. A no-op when no launch is being
/// recorded: clud still runs when the state directory is unavailable.
pub fn record_failure_reason(reason: impl std::fmt::Display) {
    if let Some(pending) = failure_slot().as_mut() {
        pending.reason = Some(reason.to_string());
    }
}

/// Exit codes that say nothing about whether the gateway was reached (#1020).
///
/// These are exactly the values `runner_exit::normalize_exit_code` produces for
/// a signal death -- SIGINT (130), SIGKILL (137), SIGTERM (143) -- and the same
/// values a shell reports for `128 + signo`. A child that was signalled did not
/// "fail before reaching the gateway": it was stopped, and whether it would
/// have got there is unknowable. Calling that a failure puts a failure label on
/// a user-initiated cancel, in the one field #998 added to stop exit codes being
/// misread.
///
/// The commonest shape by far is a PTY launch where the user reads the banner,
/// changes their mind, and hits Ctrl+C before sending a turn: the bridge counted
/// zero turns and the child exited 130. The subprocess runner never reached this
/// classification for that case, because `run_plan_subprocess` returns 130 from
/// its `ProcessOutcome::Interrupted` arm first. The PTY runner has no equivalent
/// guard and cannot get one from clud's `interrupted` flag -- raw mode sends the
/// Ctrl+C to the child, so clud's own signal handler never fires -- which is why
/// the fix lives here, on the exit code, rather than in the runner.
///
/// Deliberately just these three rather than the whole `128 <` range: a program
/// is free to exit 131 of its own accord, and over-broad exclusions would eat
/// real diagnoses. `normalize_exit_code_signal_outputs_are_all_excluded` in
/// `runner_exit.rs` fails if that mapping grows a case this list does not cover.
pub const SIGNAL_EXIT_CODES: [i32; 3] = [130, 137, 143];

/// The reason for the #995 failure class: a bridge-routed launch that exited
/// non-zero without ever asking the bridge for a turn.
///
/// #998 gave the record a reason for the failures clud raises itself, which
/// leaves `None` for a child that died on its own. The wedge that motivated
/// the issue is exactly that shape -- the bridge starts cleanly and the harness
/// exits at model resolution -- so the record still explained nothing for the
/// launch it was built for. clud cannot see that child's stderr, but the bridge
/// can answer a narrower question: did the harness ever ask us for a turn? A
/// launch that exits non-zero having asked for none failed before reaching the
/// gateway, which is the fact #995 took a multi-log hunt to establish.
///
/// `None` on a clean exit, on a launch that is not bridge-routed, when the
/// harness did reach the bridge -- `bridge.jsonl` owns that story -- and on a
/// signal death, see [`SIGNAL_EXIT_CODES`].
pub fn silent_bridge_reason(turn_requests: Option<usize>, exit_code: i32) -> Option<String> {
    if exit_code == 0 || turn_requests != Some(0) || SIGNAL_EXIT_CODES.contains(&exit_code) {
        return None;
    }
    Some(format!(
        "backend exited {exit_code} without sending the clud bridge a message request; it failed before reaching the gateway"
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub id: String,
    pub source: String,
    pub clud_pid: u32,
    pub backend: String,
    pub launch_mode: String,
    pub cwd: Option<String>,
    pub repo_root: Option<String>,
    pub command: Vec<String>,
    pub clud_argv: Vec<String>,
    pub launched_at_ms: u64,
    #[serde(default)]
    pub exited_at_ms: Option<u64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Why the launch failed, when clud raised the failure itself and already
    /// had a message for it (#998). `None` for a healthy exit and for a child
    /// that died on its own -- the backend's stderr is inherited by the
    /// terminal, never piped, so clud does not see it.
    #[serde(default)]
    pub failure_reason: Option<String>,
    /// The metered-provider warning printed at launch, when the operator
    /// enabled a pay-per-token route, so a later reader cannot miss it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metered_warning: Option<String>,
}

impl LaunchRecord {
    pub fn duration_ms(&self) -> Option<u64> {
        self.exited_at_ms
            .map(|end| end.saturating_sub(self.launched_at_ms))
    }
}

#[derive(Debug)]
pub struct LaunchLogHandle {
    state_dir: PathBuf,
    id: String,
}

impl LaunchLogHandle {
    pub fn finish(&self, exit_code: i32) {
        if let Err(err) = finish_launch(&self.state_dir, &self.id, exit_code) {
            eprintln!("[clud] warning: failed to record launch exit: {err}");
        }
    }
}

pub fn start_launch(
    state_dir: &Path,
    plan: &LaunchPlan,
    source: &str,
) -> io::Result<LaunchLogHandle> {
    let launched_at_ms = unix_millis_now();
    let id = format!("{launched_at_ms}-{}", std::process::id());
    let cwd = launch_cwd(plan);
    let repo_root = cwd.as_deref().and_then(repo_root_for_cwd);
    let (command, clud_argv) =
        redacted_record_argv(&plan.command, &std::env::args().collect::<Vec<_>>());
    let record = LaunchRecord {
        id: id.clone(),
        source: source.to_string(),
        clud_pid: std::process::id(),
        backend: plan.backend.executable_name().to_string(),
        launch_mode: plan.launch_mode.as_str().to_string(),
        cwd,
        repo_root,
        command,
        clud_argv,
        launched_at_ms,
        exited_at_ms: None,
        exit_code: None,
        failure_reason: None,
        metered_warning: metered_warning_slot().clone(),
    };
    write_record(state_dir, &record)?;
    *failure_slot() = Some(PendingFailure {
        launch_id: id.clone(),
        reason: None,
    });
    prune_old_records(state_dir);
    Ok(LaunchLogHandle {
        state_dir: state_dir.to_path_buf(),
        id,
    })
}

fn redacted_record_argv(command: &[String], argv: &[String]) -> (Vec<String>, Vec<String>) {
    (
        crate::secret_redaction::redact_args(command),
        crate::secret_redaction::redact_args(argv),
    )
}

pub fn finish_launch(state_dir: &Path, id: &str, exit_code: i32) -> io::Result<()> {
    let path = record_path(state_dir, id);
    let bytes = fs::read(&path)?;
    let mut record: LaunchRecord = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    record.exited_at_ms = Some(unix_millis_now());
    record.exit_code = Some(exit_code);
    record.failure_reason = take_failure_reason(id).map(|reason| sanitize_failure_reason(&reason));
    write_record(state_dir, &record)
}

/// Consume the pending reason, but only if it was raised for `id`.
fn take_failure_reason(id: &str) -> Option<String> {
    let mut slot = failure_slot();
    if slot.as_ref().is_some_and(|pending| pending.launch_id == id) {
        slot.take().and_then(|pending| pending.reason)
    } else {
        None
    }
}

/// Scrub then bound, in that order -- bounding first could bisect a word and
/// hide a marker from the scrub.
///
/// The reasons recorded today come from `BridgeError`'s `Display`, which is
/// documented to carry no endpoint or token text, so this is a backstop
/// against a future message that interpolates one, not the primary control.
/// It covers the *spellings* #995 named -- a bearer token and an
/// `ANTHROPIC_*` value -- and makes no claim to catch every shape those two
/// values could take.
fn sanitize_failure_reason(reason: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut after_bearer = false;
    for word in reason.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        let core = lower.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        // A bare `bearer` flags the *next* word; `bearer=tok` carries the
        // token in the same word, so that word goes whole.
        let glued_bearer = core != "bearer" && core.contains("bearer");
        let secret =
            after_bearer || word.starts_with("sk-") || lower.contains("anthropic_") || glued_bearer;
        after_bearer = core == "bearer";
        words.push(if secret {
            "[redacted]".to_string()
        } else {
            word.to_string()
        });
    }
    words
        .join(" ")
        .chars()
        .take(MAX_FAILURE_REASON_CHARS)
        .collect()
}

pub fn read_recent(state_dir: &Path) -> Vec<LaunchRecord> {
    let dir = launches_dir(state_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<LaunchRecord>(&bytes) else {
            continue;
        };
        records.push(record);
    }
    records.sort_by_key(|record| std::cmp::Reverse(record.launched_at_ms));
    records.truncate(MAX_RECORDS);
    records
}

pub fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn launch_cwd(plan: &LaunchPlan) -> Option<String> {
    plan.cwd.clone().or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
    })
}

pub fn repo_root_for_cwd(cwd: &str) -> Option<String> {
    let cwd = PathBuf::from(cwd);
    let root = loop_spec::git_root_from(&cwd);
    if root.join(".git").exists() {
        Some(root.display().to_string())
    } else {
        None
    }
}

fn write_record(state_dir: &Path, record: &LaunchRecord) -> io::Result<()> {
    let path = record_path(state_dir, &record.id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
    fs::write(path, bytes)
}

fn launches_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(DIR_NAME)
}

fn record_path(state_dir: &Path, id: &str) -> PathBuf {
    launches_dir(state_dir).join(format!("{id}.json"))
}

fn prune_old_records(state_dir: &Path) {
    let dir = launches_dir(state_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    if paths.len() <= MAX_RECORDS {
        return;
    }
    paths.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });
    let remove_count = paths.len().saturating_sub(MAX_RECORDS);
    for path in paths.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialized_launch_argv_masks_keys_but_keeps_non_secret_arguments() {
        let key = "sk-0123456789abcdef0123456789abcdef";
        let command = vec!["claude".to_string(), key.to_string()];
        let argv = vec!["clud".to_string(), format!("--deepseek={key}")];
        let (command, clud_argv) = redacted_record_argv(&command, &argv);
        let encoded = serde_json::to_string(&serde_json::json!({
            "command": command,
            "clud_argv": clud_argv,
        }))
        .unwrap();
        assert!(encoded.contains("****cdef"));
        assert!(encoded.contains("claude"));
        assert!(!encoded.contains(key));
    }

    /// The ~200 records already on disk predate `failure_reason`; `#[serde(default)]`
    /// is what keeps them readable, and this is the assertion that says so.
    #[test]
    fn record_without_failure_reason_still_deserializes() {
        let json = r#"{
            "id": "1700000000000-1",
            "source": "direct",
            "clud_pid": 1,
            "backend": "codex",
            "launch_mode": "subprocess",
            "cwd": null,
            "repo_root": null,
            "command": ["codex"],
            "clud_argv": ["clud"],
            "launched_at_ms": 1700000000000,
            "exited_at_ms": 1700000001000,
            "exit_code": 1
        }"#;
        let record: LaunchRecord = serde_json::from_str(json).expect("legacy record");
        assert_eq!(record.failure_reason, None);
    }

    /// #995's launch: bridge-routed `claude --model clud-claude-codex-sol`,
    /// exit 1 in 934 ms, no `bridge.jsonl` at all -- which is zero turn
    /// requests. The failures clud raises itself never fired for it, so
    /// without this classification the record is still a bare exit code.
    #[test]
    fn a_bridge_routed_exit_that_never_asked_for_a_turn_is_classified() {
        let reason = silent_bridge_reason(Some(0), 1).expect("silence must be classified");
        assert!(
            reason.contains("without sending the clud bridge"),
            "{reason}"
        );
        // A launch that did reach the bridge, one with no bridge at all, and a
        // clean exit are each somebody else's story.
        assert_eq!(silent_bridge_reason(Some(3), 1), None);
        assert_eq!(silent_bridge_reason(None, 1), None);
        assert_eq!(silent_bridge_reason(Some(0), 0), None);
    }

    /// Issue #1020: a signal death is a cancel, not a diagnosis.
    ///
    /// The reported shape is a PTY launch — `clud --codex --harness claude`
    /// under `foreground.pty`, or `clud loop` off Windows — where the user
    /// reads the banner, changes their mind, and hits Ctrl+C before ever
    /// sending a turn. The bridge counted zero turns and the child exited 130,
    /// so the pre-#1020 rule wrote "it failed before reaching the gateway" onto
    /// a user-initiated cancel.
    ///
    /// This became far easier to hit once #1101 made a PTY Ctrl+C actually
    /// produce 130: before that fix, a kitty-protocol terminal never reached
    /// `interrupt_pty_process` at all.
    #[test]
    fn a_signal_death_is_not_a_gateway_failure() {
        for code in SIGNAL_EXIT_CODES {
            assert_eq!(
                silent_bridge_reason(Some(0), code),
                None,
                "exit {code} is a signal death; clud cannot say whether the gateway was reached"
            );
        }
    }

    /// The exclusion is three exact codes, not a `> 128` range. A program is
    /// free to exit 131 or 129 on its own, and swallowing those would lose the
    /// real #995 diagnosis they were added for.
    #[test]
    fn neighbouring_exit_codes_are_still_classified() {
        for code in [1, 2, 127, 129, 131, 136, 142, 144] {
            assert!(
                silent_bridge_reason(Some(0), code).is_some(),
                "exit {code} is not a signal death clud produces; it must keep its diagnosis"
            );
        }
    }

    /// The whole point of #998: an exit that clud caused carries the reason,
    /// bounded and with the two secret shapes #995 named scrubbed out.
    ///
    /// Sole test touching `FAILURE_REASON`; extra scrub cases go through
    /// `sanitize_failure_reason` directly so nothing races the global slot.
    #[test]
    fn finish_launch_stores_a_bounded_redacted_failure_reason() {
        let dir = tempfile::tempdir().unwrap();
        let record = LaunchRecord {
            id: "1700000000000-2".to_string(),
            source: "direct".to_string(),
            clud_pid: 2,
            backend: "claude".to_string(),
            launch_mode: "pty".to_string(),
            cwd: None,
            repo_root: None,
            command: vec!["claude".to_string()],
            clud_argv: vec!["clud".to_string()],
            launched_at_ms: 1_700_000_000_000,
            exited_at_ms: None,
            exit_code: None,
            failure_reason: None,
            metered_warning: None,
        };
        write_record(dir.path(), &record).unwrap();
        *failure_slot() = Some(PendingFailure {
            launch_id: record.id.clone(),
            reason: None,
        });

        record_failure_reason(format_args!(
            "failed to start provider bridge: Bearer tok-1234 ANTHROPIC_API_KEY=secret sk-ant-abc {}",
            "y".repeat(MAX_FAILURE_REASON_CHARS)
        ));
        finish_launch(dir.path(), &record.id, 1).unwrap();

        let stored = read_recent(dir.path()).remove(0);
        let reason = stored.failure_reason.expect("reason recorded");
        assert_eq!(reason.chars().count(), MAX_FAILURE_REASON_CHARS, "{reason}");
        assert!(
            reason.starts_with("failed to start provider bridge:"),
            "{reason}"
        );
        for secret in ["tok-1234", "ANTHROPIC_API_KEY", "sk-ant-abc"] {
            assert!(!reason.contains(secret), "{secret} leaked: {reason}");
        }
        // A later launch that exits cleanly must not inherit the reason.
        finish_launch(dir.path(), &record.id, 0).unwrap();
        assert_eq!(read_recent(dir.path()).remove(0).failure_reason, None);
    }

    /// Scrub edges, exercised through the pure function so nothing races the
    /// global slot the test above owns.
    #[test]
    fn sanitize_redacts_glued_and_lowercased_secret_spellings() {
        assert_eq!(
            sanitize_failure_reason("bearer=tok-1234 anthropic_api_key=v ok"),
            "[redacted] [redacted] ok"
        );
        // A word that merely contains a marker substring survives.
        assert_eq!(sanitize_failure_reason("task-list risk"), "task-list risk");
    }
}

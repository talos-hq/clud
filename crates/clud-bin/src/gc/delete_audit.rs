//! Pre-deletion audit trail for clud's destructive GC/sweep paths (issue
//! #893).
//!
//! #893 reported a live repo directory vanishing mid-session with nothing in
//! any log naming what removed it. Whatever the culprit turns out to be, the
//! durable fix has two halves and this module is one of them: every deletion
//! path that could touch user data writes one JSONL line to
//! `<state>/gc-audit.jsonl` **immediately before** acting, naming the sweep
//! site, the exact filesystem path, and the rule that selected it (with
//! credential shapes redacted from free-text explanations). A future
//! incident is then answerable by grepping one file instead of being
//! reconstructed from an unrelated daemon crash log.
//!
//! Scope boundary: sites that remove only directories clud itself created
//! moments earlier inside the same call (the soldr-download temp dir, the
//! cmd-installer staging dir, the git-bash extraction dir, voice capture
//! dirs) are deliberately not wired — they cannot match user data by
//! construction.
//!
//! Synchronous append is deliberate, unlike the reaper log's buffering
//! ([`crate::reap_log`]): deletions here are rare (daily sweeps), while a
//! crash mid-deletion is exactly when the line matters most, so per-line
//! flush cost buys durability where it counts. Cache sweeps retain
//! best-effort logging. Source-tree removals use `record_required` and refuse
//! to proceed if their audit record cannot be written and synced (#1790).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::daemon::ENV_STATE_DIR;

/// Audit log file name, resolved against the same state dir the daemon uses.
pub const AUDIT_LOG_FILE: &str = "gc-audit.jsonl";

/// Resolve `<state-dir>/gc-audit.jsonl`, honoring [`ENV_STATE_DIR`] the same
/// way `daemon::paths::default_state_dir` does. `None` when no home dir can
/// be resolved (headless/misconfigured env). Required source-removal audits
/// refuse to proceed in that case.
pub fn audit_log_path() -> Option<PathBuf> {
    let state_dir = match std::env::var(ENV_STATE_DIR) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => crate::home::user_home()?.join(".clud").join("state"),
    };
    Some(state_dir.join(AUDIT_LOG_FILE))
}

/// Record one pending deletion. Best-effort: never panics, never errors.
///
/// Call sites must invoke this *before* the destructive call, per #893.
///
/// - `site` — the code path performing the deletion (e.g. `gc.target-sweep`).
/// - `path` — the exact directory about to be removed.
/// - `rule` — why it was selected (e.g. `target-sweep stale>14d`).
pub fn record(site: &str, path: &Path, rule: &str) {
    // Under `cfg(test)` a write goes only to the log of the `StateDirGuard`
    // held by *this* thread (#1450). An unguarded test's deletion is dropped
    // instead of landing in a sibling test's log (the env var is
    // process-global) or in the developer's real state dir.
    #[cfg(test)]
    let Some(log_path) = TEST_AUDIT_LOG.with(|cell| cell.borrow().clone()) else {
        return;
    };
    #[cfg(not(test))]
    let Some(log_path) = audit_log_path() else {
        return;
    };
    record_at(&log_path, site, path, rule);
}

/// Required audit for source-tree reclamation (#1790). Refuse the action
/// if its evidence cannot be persisted; best-effort cache sweeps use `record`.
pub(crate) fn record_required(site: &str, path: &Path, rule: &str) -> std::io::Result<()> {
    #[cfg(test)]
    let Some(log_path) = TEST_AUDIT_LOG.with(|cell| cell.borrow().clone()) else {
        return Ok(());
    };
    #[cfg(not(test))]
    let log_path = audit_log_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "GC audit path unavailable")
    })?;
    record_required_at(&log_path, site, path, rule)
}

fn record_required_at(log_path: &Path, site: &str, path: &Path, rule: &str) -> std::io::Result<()> {
    let rule = crate::secret_redaction::redact_for_log(rule);
    let line = serde_json::json!({
        "ts_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
        "event": "delete",
        "site": site,
        "path": path,
        "rule": rule,
    });
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    file.write_all(format!("{line}\n").as_bytes())?;
    file.sync_data()
}

/// Testable core of [`record`] — appends one JSONL line synchronously.
pub fn record_at(log_path: &Path, site: &str, path: &Path, rule: &str) {
    let rule = crate::secret_redaction::redact_for_log(rule);
    let line = serde_json::json!({
        "ts_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "event": "delete",
        "site": site,
        "path": path.display().to_string(),
        "rule": rule,
    });
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) else {
        return;
    };
    let _ = file.write_all(format!("{line}\n").as_bytes());
}

#[cfg(test)]
thread_local! {
    /// The audit log [`record`] writes to in tests; set only by
    /// [`StateDirGuard`] on the thread that holds it (#1450).
    static TEST_AUDIT_LOG: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Shared test helper: swap [`ENV_STATE_DIR`] for the duration of a test so
/// sweeps can be pointed at an injectable audit log. `std::env` is
/// process-global, so all users serialize through one mutex. [`record`] in
/// tests writes only for the thread holding a guard, so an unguarded test's
/// deletion never lands in the guarded test's log (#1450).
#[cfg(test)]
pub(crate) struct StateDirGuard {
    prior: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl StateDirGuard {
    pub(crate) fn set(dir: &Path) -> Self {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let lock = M
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os(ENV_STATE_DIR);
        std::env::set_var(ENV_STATE_DIR, dir);
        TEST_AUDIT_LOG.with(|cell| *cell.borrow_mut() = Some(dir.join(AUDIT_LOG_FILE)));
        Self { prior, _lock: lock }
    }
}

#[cfg(test)]
impl Drop for StateDirGuard {
    fn drop(&mut self) {
        TEST_AUDIT_LOG.with(|cell| *cell.borrow_mut() = None);
        match self.prior.take() {
            Some(v) => std::env::set_var(ENV_STATE_DIR, v),
            None => std::env::remove_var(ENV_STATE_DIR),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn record_redaction_required_gc_audit() {
        assert_rule_redaction(true);
    }

    #[test]
    fn record_redaction_best_effort_gc_audit() {
        assert_rule_redaction(false);
    }

    fn assert_rule_redaction(required: bool) {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("audit.jsonl");
        let secret = format!("ghp_{}", "A".repeat(36));
        let target = tmp.path().join(&secret);
        fs::create_dir(&target).unwrap();
        let rule = format!("override explanation: command --token {secret}");
        let write = |rule: &str| {
            if required {
                record_required_at(&log, &secret, &target, rule).unwrap();
            } else {
                record_at(&log, &secret, &target, rule);
            }
        };
        write(&rule);
        let text = fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 1);
        let mut actual: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert!(
            !actual["rule"].to_string().contains(&secret),
            "rule persisted verbatim: {}",
            actual["rule"]
        );
        assert!(actual["rule"]
            .as_str()
            .unwrap()
            .starts_with("override explanation: command --token "));
        actual["rule"] = serde_json::json!(rule);
        assert_eq!(
            actual,
            serde_json::json!({"ts_ms":actual["ts_ms"],
            "event":"delete", "site":secret, "path":target, "rule":rule})
        );
        assert!(target.is_dir());
        write("ordinary selection rule");
        let text = fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 2);
        let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(
            last,
            serde_json::json!({"ts_ms":last["ts_ms"],
            "event":"delete", "site":secret, "path":target, "rule":"ordinary selection rule"})
        );
        assert!(target.is_dir());
    }

    #[test]
    fn record_at_writes_parseable_jsonl_with_site_path_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("gc-audit.jsonl");
        record_at(
            &log,
            "gc.target-sweep",
            Path::new("/dev/repo/target"),
            "stale>14d",
        );
        let body = fs::read_to_string(&log).unwrap();
        assert_eq!(body.lines().count(), 1);
        let parsed: Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(parsed["event"], "delete");
        assert_eq!(parsed["site"], "gc.target-sweep");
        // Forward slashes survive round-trip; the display form is what makes
        // the path greppable against `dir /s` or `find` output.
        assert!(parsed["path"].as_str().unwrap().contains("repo"));
        assert!(parsed["path"].as_str().unwrap().ends_with("target"));
        assert_eq!(parsed["rule"], "stale>14d");
        assert!(parsed["ts_ms"].as_u64().is_some());
    }

    #[test]
    fn source_deletion_audit_must_be_durable_or_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("audit.jsonl");
        record_required_at(
            &log,
            "gc.entry-delete",
            Path::new("source"),
            "verified clean",
        )
        .unwrap();
        let data: Value = serde_json::from_str(fs::read_to_string(log).unwrap().trim()).unwrap();
        assert_eq!(data["rule"], "verified clean");
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, "file").unwrap();
        assert!(record_required_at(
            &blocker.join("audit"),
            "gc.entry-delete",
            tmp.path(),
            "rule"
        )
        .is_err());
    }

    #[test]
    fn successive_records_append_one_line_each() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("nested").join("gc-audit.jsonl");
        record_at(&log, "a", Path::new("/x"), "r1");
        record_at(&log, "b", Path::new("/y"), "r2");
        let body = fs::read_to_string(&log).unwrap();
        assert_eq!(body.lines().count(), 2, "parent dirs auto-created");
    }

    #[test]
    fn an_unwritable_log_is_silent_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        // Parent is an existing *file* — open can never succeed.
        record_at(&blocker.join("gc-audit.jsonl"), "a", Path::new("/x"), "r");
    }

    #[test]
    fn audit_log_path_honors_the_daemon_state_dir_env() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = StateDirGuard::set(tmp.path());
        let resolved = audit_log_path().expect("env override resolves without home");
        assert_eq!(
            resolved.file_name().unwrap(),
            AUDIT_LOG_FILE,
            "same state dir convention as the daemon"
        );
        assert!(resolved.starts_with(tmp.path()));
    }

    /// #1450: in tests, `record` writes only while this thread holds a guard,
    /// so an unguarded test can neither pollute a guarded sibling's log nor
    /// the developer's real state dir.
    #[test]
    fn record_in_tests_writes_only_under_this_threads_guard() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let _guard = StateDirGuard::set(tmp.path());
            let tmp2 = tmp.path().to_path_buf();
            std::thread::spawn(move || record("t", &tmp2.join("other"), "unguarded"))
                .join()
                .unwrap();
            record("t", &tmp.path().join("mine"), "guarded");
        }
        record("t", &tmp.path().join("after"), "after-drop");
        let body = fs::read_to_string(tmp.path().join(AUDIT_LOG_FILE)).unwrap();
        assert_eq!(body.lines().count(), 1, "{body}");
        assert!(body.contains("\"guarded\""), "{body}");
    }
}

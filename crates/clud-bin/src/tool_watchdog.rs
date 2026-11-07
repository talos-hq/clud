//! Watchdog enforcement for `clud tool run`. Slice 5 of #427.
//!
//! Wraps the poll-drain loop in `tool_run.rs` with two timers:
//!
//! - **command_timeout** — wall-clock cap on the whole invocation.
//! - **progress_timeout** — abort when no output (stdout/stderr) has
//!   been observed for this duration. Skipped when the tool declares
//!   `quiet_ok: true`.
//!
//! Behavior on timer fire depends on the tool's `kill_semantics`
//! (slice 1's `BundledTool` field):
//!
//! - `Resumable` — emit an `in-progress` JSON terminal, exit 124. The
//!   world holds the state; the caller can re-invoke to resume. Exit 0
//!   is reserved for a tool that actually completed successfully.
//! - `Killable` — kill the process tree, emit an `aborted` JSON
//!   terminal, exit non-zero. The diagnostic block (last lines /
//!   process tree / open files) is slice 6 polish; V1 ships with
//!   the minimum: reason + elapsed + exit_code.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::tools::{KillSemantics, BUNDLED_TOOLS};

/// Exit status for either kind of watchdog stop. The JSON status distinguishes
/// an in-progress resumable operation from an aborted killable operation.
pub const WATCHDOG_EXIT_CODE: i32 = 124;

/// What the watchdog decided to do at a poll boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogDecision {
    /// Keep polling — neither timer has fired.
    Continue,
    /// Kill the tool process tree and emit an `aborted` terminal.
    KillAndAbort(AbortReason),
    /// Tool is resumable; emit an `in-progress` terminal and exit 124.
    ResumeLater(AbortReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReason {
    CommandTimeout,
    ProgressTimeout,
}

impl AbortReason {
    pub fn label(self) -> &'static str {
        match self {
            AbortReason::CommandTimeout => "command_timeout",
            AbortReason::ProgressTimeout => "progress_timeout",
        }
    }
}

/// Per-invocation watchdog state. Constructed once at tool start and
/// queried each poll iteration.
#[derive(Debug, Clone)]
pub struct Watchdog {
    pub kill_semantics: KillSemantics,
    /// Duration::MAX disables the command deadline (capture owns explicit timeouts).
    pub command_timeout: Duration,
    pub progress_timeout: Option<Duration>,
    pub quiet_ok: bool,
    pub started_at: Instant,
    pub last_output_at: Instant,
}

impl Watchdog {
    /// Build a watchdog for the given bundled-tool `rel_path`. Falls
    /// back to sensible defaults (Killable, 60m command timeout, no
    /// progress watchdog) when the tool isn't in `BUNDLED_TOOLS` — e.g.
    /// a user-installed tool the agent invokes by path.
    pub fn for_rel_path(rel_path: &str) -> Self {
        let now = Instant::now();
        let watchdog = if let Some(tool) = BUNDLED_TOOLS.iter().find(|t| t.rel_path == rel_path) {
            Self {
                kill_semantics: tool.kill_semantics,
                command_timeout: tool.command_timeout,
                progress_timeout: tool.progress_timeout,
                quiet_ok: tool.quiet_ok,
                started_at: now,
                last_output_at: now,
            }
        } else {
            Self {
                kill_semantics: KillSemantics::Killable,
                command_timeout: KillSemantics::Killable.default_command_timeout(),
                progress_timeout: None,
                quiet_ok: false,
                started_at: now,
                last_output_at: now,
            }
        };
        // The CLI regression test needs a short timer while exercising a
        // real bundled tool. Keep these knobs out of release binaries.
        #[cfg(debug_assertions)]
        let mut watchdog = watchdog;
        #[cfg(debug_assertions)]
        if rel_path == "github/pr_merge_watch.py" {
            if let Some(ms) = std::env::var("CLUD_TEST_TOOL_COMMAND_TIMEOUT_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
            {
                watchdog.command_timeout = Duration::from_millis(ms);
            }
            if let Some(ms) = std::env::var("CLUD_TEST_TOOL_PROGRESS_TIMEOUT_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
            {
                watchdog.progress_timeout = Some(Duration::from_millis(ms));
            }
        }
        watchdog
    }

    /// Mark that the tool emitted output. Resets the progress timer.
    pub fn note_output(&mut self) {
        self.last_output_at = Instant::now();
    }

    /// Decide what to do at this poll boundary. Returns `Continue`
    /// when neither timer has fired.
    pub fn check(&self) -> WatchdogDecision {
        let now = Instant::now();
        if self.command_timeout != Duration::MAX
            && now.duration_since(self.started_at) >= self.command_timeout
        {
            return self.fired(AbortReason::CommandTimeout);
        }
        if let Some(p) = self.progress_timeout {
            if !self.quiet_ok && now.duration_since(self.last_output_at) >= p {
                return self.fired(AbortReason::ProgressTimeout);
            }
        }
        WatchdogDecision::Continue
    }

    fn fired(&self, reason: AbortReason) -> WatchdogDecision {
        match self.kill_semantics {
            KillSemantics::Resumable | KillSemantics::ResumableWithKillableSubsteps => {
                WatchdogDecision::ResumeLater(reason)
            }
            KillSemantics::Killable => WatchdogDecision::KillAndAbort(reason),
        }
    }

    /// Render the resumable `in-progress` terminal payload as a single
    /// JSON line. Caller writes this to stderr and exits 124.
    pub fn render_in_progress(&self, reason: AbortReason) -> String {
        let elapsed = self.started_at.elapsed();
        let value = json!({
            "v": 1,
            "status": "in-progress",
            "reason": reason.label(),
            "elapsed_ms": elapsed.as_millis() as u64,
            "resume_hint": "operation still running; re-invoke the same tool with the same args to resume polling",
        });
        value.to_string()
    }

    /// Render the killable `aborted` terminal payload as a single JSON
    /// line. Slice 6 will extend this with the diagnostic block; V1
    /// ships with the minimum so callers can rely on a stable shape.
    pub fn render_aborted(&self, reason: AbortReason) -> String {
        let elapsed = self.started_at.elapsed();
        let value = json!({
            "v": 1,
            "status": "aborted",
            "reason": reason.label(),
            "elapsed_ms": elapsed.as_millis() as u64,
        });
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_watchdog(
        semantics: KillSemantics,
        command_timeout: Duration,
        progress_timeout: Option<Duration>,
        quiet_ok: bool,
    ) -> Watchdog {
        let now = Instant::now();
        Watchdog {
            kill_semantics: semantics,
            command_timeout,
            progress_timeout,
            quiet_ok,
            started_at: now,
            last_output_at: now,
        }
    }

    #[test]
    fn check_returns_continue_when_no_timer_fired() {
        let w = fixed_watchdog(
            KillSemantics::Killable,
            Duration::from_secs(60),
            None,
            false,
        );
        assert_eq!(w.check(), WatchdogDecision::Continue);
    }

    #[test]
    fn check_fires_command_timeout_when_elapsed() {
        let mut w = fixed_watchdog(
            KillSemantics::Killable,
            Duration::from_millis(0),
            None,
            false,
        );
        // started_at is now-ish; with a 0ms command_timeout the first
        // check should trip immediately.
        w.started_at = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            w.check(),
            WatchdogDecision::KillAndAbort(AbortReason::CommandTimeout)
        );
    }

    #[test]
    fn resumable_resumes_later_on_command_timeout() {
        let mut w = fixed_watchdog(
            KillSemantics::Resumable,
            Duration::from_millis(0),
            None,
            false,
        );
        w.started_at = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            w.check(),
            WatchdogDecision::ResumeLater(AbortReason::CommandTimeout)
        );
    }

    #[test]
    fn progress_timeout_fires_when_no_output() {
        let mut w = fixed_watchdog(
            KillSemantics::Killable,
            Duration::from_secs(3600),
            Some(Duration::from_millis(0)),
            false,
        );
        w.last_output_at = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            w.check(),
            WatchdogDecision::KillAndAbort(AbortReason::ProgressTimeout)
        );
    }

    #[test]
    fn quiet_ok_suppresses_progress_timeout() {
        let mut w = fixed_watchdog(
            KillSemantics::Killable,
            Duration::from_secs(3600),
            Some(Duration::from_millis(0)),
            true, // quiet_ok
        );
        w.last_output_at = Instant::now() - Duration::from_secs(1);
        assert_eq!(w.check(), WatchdogDecision::Continue);
    }

    #[test]
    fn note_output_resets_progress_timer() {
        let mut w = fixed_watchdog(
            KillSemantics::Killable,
            Duration::from_secs(3600),
            Some(Duration::from_secs(1)),
            false,
        );
        w.last_output_at = Instant::now() - Duration::from_secs(5);
        // Would fire before note_output:
        assert_eq!(
            w.check(),
            WatchdogDecision::KillAndAbort(AbortReason::ProgressTimeout)
        );
        w.note_output();
        // Now reset, should be Continue.
        assert_eq!(w.check(), WatchdogDecision::Continue);
    }

    #[test]
    fn render_in_progress_emits_valid_json() {
        let w = fixed_watchdog(
            KillSemantics::Resumable,
            Duration::from_secs(60),
            None,
            false,
        );
        let s = w.render_in_progress(AbortReason::CommandTimeout);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "in-progress");
        assert_eq!(v["reason"], "command_timeout");
        assert_eq!(v["v"], 1);
        assert!(v["resume_hint"].is_string());
    }

    #[test]
    fn render_aborted_emits_valid_json() {
        let w = fixed_watchdog(
            KillSemantics::Killable,
            Duration::from_secs(60),
            None,
            false,
        );
        let s = w.render_aborted(AbortReason::ProgressTimeout);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "aborted");
        assert_eq!(v["reason"], "progress_timeout");
        assert_eq!(v["v"], 1);
    }

    #[test]
    fn for_rel_path_unknown_tool_uses_killable_defaults() {
        let w = Watchdog::for_rel_path("does/not/exist.py");
        assert_eq!(w.kill_semantics, KillSemantics::Killable);
        assert_eq!(w.command_timeout, Duration::from_secs(60 * 60));
        assert_eq!(w.progress_timeout, None);
        assert!(!w.quiet_ok);
    }

    #[test]
    fn for_rel_path_known_tool_uses_registry_values() {
        // pr_merge_watch.py is in BUNDLED_TOOLS as Resumable per slice 1.
        let w = Watchdog::for_rel_path("github/pr_merge_watch.py");
        assert_eq!(w.kill_semantics, KillSemantics::Resumable);
        assert_eq!(w.command_timeout, Duration::from_secs(3660));
    }
}

#[cfg(test)]
mod reporter_deadline_tests {
    use super::*;

    #[test]
    fn reporter_has_no_implicit_deadline() {
        let mut watchdog = Watchdog::for_rel_path("diagnostics/test_report.py");
        watchdog.started_at = Instant::now() - Duration::from_secs(172_800);
        assert_eq!(watchdog.check(), WatchdogDecision::Continue);
    }
}

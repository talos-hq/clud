//! Reaper accounting and its per-session log (#673 Phases 0 and 5).
//!
//! Reaping is destructive and, until now, silent: the only trace a session
//! left was a structured event per decision in the shared daemon event log,
//! interleaved with everything else the daemon writes. That is why #651 could
//! be closed as fixed while the same symptom kept growing — nobody could see
//! the backlog.
//!
//! Two surfaces, both cheap:
//!
//! - **[`ReapCounters`]** — the Phase 0 measurement series. `known.len()`,
//!   backlog size, environment reads, reconcile ticks, and the decision
//!   census. Reported under `--verbose` and in the exit summary.
//! - **[`ReapLog`]** — one JSONL line per *mutation*, alongside the session's
//!   other artifacts. Never a line for a no-op pass.
//!
//! ## Why the writer is buffered
//!
//! #544 found per-operation synchronous JSONL flushes to be an idle-CPU cost
//! in their own right. A reap log that fsynced per decision would trade one
//! idle-CPU finding for another, so this one accumulates and flushes on a size
//! or time threshold, and once at exit.
//!
//! ## The two reconciliation identities
//!
//! ```text
//! shell_exits_observed == finalized + abandoned + still_pending_at_exit
//! decisions_emitted    == reaped + spared
//! ```
//!
//! They are deliberately separate, and `tracked` belongs to neither, because
//! the populations are disjoint: most spawned processes are never reap
//! candidates (only shell images become triggers); abandoned identities are
//! *triggers* while kill targets are their *descendants*; a reap decision can
//! be downgraded to a spare at execution time; and a kill takes a whole
//! subtree without emitting a decision per descendant. An identity that
//! equated them would be arithmetic that cannot hold.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Lines buffered before a flush is due.
const FLUSH_LINES: usize = 64;

/// Time buffered before a flush is due.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// The reaper's crash-surviving checkpoint is deliberately much less frequent
/// than its buffered event log. It exists for the failure mode where the
/// process never reaches `Drop` (a watchdog reset), so the exit summary is
/// unavailable.
const FLIGHT_RECORDER_INTERVAL: Duration = Duration::from_secs(5);

/// What a reap event did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReapAction {
    Reaped,
    Spared,
    Abandoned,
}

impl ReapAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reaped => "reaped",
            Self::Spared => "spared",
            Self::Abandoned => "abandoned",
        }
    }
}

/// When the event happened relative to the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReapPhase {
    /// While the session was running, off the completion port.
    Runtime,
    /// During the foreground exit sweep.
    Exit,
}

impl ReapPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Exit => "exit",
        }
    }
}

/// One line of the reap log.
#[derive(Clone, Debug)]
pub struct ReapEvent {
    pub ts_ms: u64,
    pub pid: Option<u32>,
    /// Immediate parent from the process-topology snapshot. Together with the
    /// trigger PID this makes every destructive decision attributable.
    pub parent_pid: Option<u32>,
    pub start_time: Option<u64>,
    pub image_name: Option<String>,
    pub action: ReapAction,
    /// Reused verbatim from `ReapDecisionReason`, so the log and the daemon
    /// event stream name the same thing the same way.
    pub reason: &'static str,
    pub phase: ReapPhase,
}

impl ReapEvent {
    fn to_json_line(&self) -> String {
        serde_json::json!({
            "ts_ms": self.ts_ms,
            "pid": self.pid,
            "parent_pid": self.parent_pid,
            "start_time": self.start_time,
            "image_name": self.image_name,
            "action": self.action.as_str(),
            "reason": self.reason,
            "phase": self.phase.as_str(),
        })
        .to_string()
    }
}

/// The Phase 0 measurement series plus the decision census.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ReapCounters {
    /// Every `NEW_PROCESS` observation. A standalone denominator that belongs
    /// to neither reconciliation identity.
    pub tracked: u64,
    /// Shell-image exits that entered the backlog.
    pub shell_exits_observed: u64,
    pub finalized: u64,
    pub abandoned: u64,
    pub decisions_emitted: u64,
    pub reaped_runtime: u64,
    pub reaped_at_exit: u64,
    pub spared: u64,
    /// Reconcile passes that got past the empty-backlog guard.
    pub reconcile_passes: u64,
    /// Completion-port iterations, including the quiet-period timeouts.
    pub ticks: u64,
    /// Full host process-table enumerations (`CreateToolhelp32Snapshot` over
    /// every process on the machine), counted from **every** call site.
    ///
    /// The number #706 exists to drive down. It used to be **one per
    /// completion-port message** — at a measured ~178 process spawns/second
    /// and ~20 ms per enumeration, that is ~3.6 cores of pure kernel time per
    /// session, and it multiplies by concurrent sessions because each scan is
    /// `O(all processes on the host)`. Messages are now folded into one batch
    /// per drain and the batch shares a single enumeration, so the dominant
    /// term should track *drains*, not *events*.
    ///
    /// Incremented inside `snapshot()` itself rather than at the call sites,
    /// so the batch path, `register_backend`, the exit sweep and the
    /// quiet-period retry are all visible. Counting only the batch path (as
    /// #707 first did) hid the retry, which fires on the 200 ms timeout
    /// whenever a PID is still unresolved.
    ///
    /// **`host_scans` may exceed `ticks`**: a single tick can drive both a
    /// retry scan and a batch scan. What must hold is that the *batch* path
    /// contributes at most one per drain, which is what `peak_batch` shows.
    pub host_scans: u64,
    /// Largest completion-port batch folded into a single drain. A value
    /// well above 1 is the fix in #706 doing its job under churn.
    pub peak_batch: u64,
    /// Process environment blocks actually read. The number #673 exists to
    /// drive to ~zero in steady state — it used to be 442 per process exit.
    pub env_reads: u64,
    /// High-water marks for the two maps whose growth tracked session age.
    pub peak_known: u64,
    pub peak_backlog: u64,
    /// Job notifications for a PID Toolhelp could not resolve before it
    /// exited (#689).
    ///
    /// These are **expected** under process churn — the path handles them and
    /// fails closed — so they are diagnostics, not incidents. They used to be
    /// enumerated one synchronous `log_structured_event` at a time into the
    /// shared `daemon-events.jsonl`, where at ~300 writes/min they were 98.8%
    /// of the log and rotated every other producer's events out before anyone
    /// could read them. The per-PID lines now go to this session's buffered
    /// `reap.jsonl`; this counter is what makes the total recoverable without
    /// reading any of them.
    pub metadata_misses: u64,
    /// Full-host table scans deliberately skipped by adaptive backoff. A skip
    /// only delays metadata collection; it cannot produce a reap decision.
    pub host_scans_deferred: u64,
}

impl ReapCounters {
    pub fn reaped(&self) -> u64 {
        self.reaped_runtime + self.reaped_at_exit
    }

    /// Fold `other` into `self`.
    ///
    /// `ACTIVE_PROCESS_ZERO` clears the tracker's accounting mid-session, so
    /// per-epoch counts are accumulated into session totals before the reset
    /// rather than being lost.
    pub fn absorb(&mut self, other: &ReapCounters) {
        self.tracked += other.tracked;
        self.shell_exits_observed += other.shell_exits_observed;
        self.finalized += other.finalized;
        self.abandoned += other.abandoned;
        self.decisions_emitted += other.decisions_emitted;
        self.reaped_runtime += other.reaped_runtime;
        self.reaped_at_exit += other.reaped_at_exit;
        self.spared += other.spared;
        self.reconcile_passes += other.reconcile_passes;
        self.ticks += other.ticks;
        self.host_scans += other.host_scans;
        self.peak_batch = self.peak_batch.max(other.peak_batch);
        self.env_reads += other.env_reads;
        self.peak_known = self.peak_known.max(other.peak_known);
        self.peak_backlog = self.peak_backlog.max(other.peak_backlog);
        self.metadata_misses += other.metadata_misses;
        self.host_scans_deferred += other.host_scans_deferred;
    }

    pub fn observe_sizes(&mut self, known: usize, backlog: usize) {
        self.peak_known = self.peak_known.max(known as u64);
        self.peak_backlog = self.peak_backlog.max(backlog as u64);
    }

    /// `shell_exits_observed == finalized + abandoned + still_pending`.
    pub fn still_pending(&self) -> u64 {
        self.shell_exits_observed
            .saturating_sub(self.finalized + self.abandoned)
    }

    /// Both identities, checkable by a test and by a reader of the summary.
    pub fn identities_hold(&self) -> bool {
        self.finalized + self.abandoned + self.still_pending() == self.shell_exits_observed
            && self.reaped() + self.spared == self.decisions_emitted
    }

    /// Nothing was tracked, so there is nothing worth printing.
    pub fn is_silent(&self) -> bool {
        self.tracked == 0
            && self.shell_exits_observed == 0
            && self.decisions_emitted == 0
            && self.metadata_misses == 0
    }

    /// The human-readable exit summary, or `None` when nothing was tracked.
    pub fn summary_lines(&self, log_path: Option<&Path>) -> Option<Vec<String>> {
        if self.is_silent() {
            return None;
        }
        let mut lines = vec![
            format!(
                "[clud] reaper: {} tracked | {} shell exits | {} finalized, {} abandoned, {} still pending",
                self.tracked,
                self.shell_exits_observed,
                self.finalized,
                self.abandoned,
                self.still_pending(),
            ),
            format!(
                "[clud] reaper: {} reaped ({} runtime + {} at exit) | {} spared",
                self.reaped(),
                self.reaped_runtime,
                self.reaped_at_exit,
                self.spared,
            ),
        ];
        if self.metadata_misses > 0 {
            // One line per session, replacing one shared-log write per miss.
            lines.push(format!(
                "[clud] reaper: {} process(es) exited before their metadata                  resolved (spared; see reaper log)",
                self.metadata_misses,
            ));
        }
        if let Some(path) = log_path {
            lines.push(format!("[clud] reaper log: {}", path.display()));
        }
        Some(lines)
    }

    /// The Phase 0 measurement line, for `--verbose`.
    pub fn measurement_line(&self) -> String {
        format!(
            "[clud] reaper: ticks={} passes={} env_reads={} peak_known={} peak_backlog={}              metadata_misses={} host_scans={} host_scans_deferred={} peak_batch={}",
            self.ticks,
            self.reconcile_passes,
            self.env_reads,
            self.peak_known,
            self.peak_backlog,
            self.metadata_misses,
            self.host_scans,
            self.host_scans_deferred,
            self.peak_batch,
        )
    }
}

/// A tiny, durable checkpoint kept beside `reap.jsonl`.
///
/// Unlike [`ReapLog`], this has one fixed-size JSON payload and syncs at most
/// once per five seconds. That makes it useful after a watchdog reset without
/// putting synchronous IO on the per-notification path.
#[derive(Debug)]
pub struct ReapFlightRecorder {
    path: PathBuf,
    /// `None` until the first write. Seeding it with `now - interval` panicked
    /// when the monotonic clock was younger than the interval, e.g. an
    /// autostart within 5s of boot (#1913).
    last_write: Option<Instant>,
    disabled: bool,
}

impl ReapFlightRecorder {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            last_write: None,
            disabled: false,
        }
    }

    pub fn checkpoint(&mut self, counters: &ReapCounters) {
        let recent = self
            .last_write
            .is_some_and(|last| last.elapsed() < FLIGHT_RECORDER_INTERVAL);
        if self.disabled || recent {
            return;
        }
        if self.write(counters).is_err() {
            self.disabled = true;
        }
        self.last_write = Some(Instant::now());
    }

    fn write(&self, counters: &ReapCounters) -> std::io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("missing parent"))?;
        fs::create_dir_all(parent)?;
        let temp = self.path.with_extension("tmp");
        let body = serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "written_at_unix_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
            "counters": counters,
        }))
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        let mut file = File::create(&temp)?;
        file.write_all(&body)?;
        file.sync_all()?;
        if self.path.exists() {
            let _ = fs::remove_file(&self.path);
        }
        fs::rename(temp, &self.path)
    }
}

/// Buffered, mutations-only JSONL writer.
///
/// Nothing is written for a pass that changed nothing, and nothing is written
/// synchronously per event.
#[derive(Debug)]
pub struct ReapLog {
    path: PathBuf,
    buffered: Vec<String>,
    last_flush: Instant,
    /// Set once a write fails, so a read-only or full disk degrades to a
    /// no-op instead of retrying on every event.
    disabled: bool,
}

impl ReapLog {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            buffered: Vec::new(),
            last_flush: Instant::now(),
            disabled: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Buffer one event, flushing if a threshold is due.
    pub fn record(&mut self, event: &ReapEvent) {
        if self.disabled {
            return;
        }
        self.buffered.push(event.to_json_line());
        if self.buffered.len() >= FLUSH_LINES || self.last_flush.elapsed() >= FLUSH_INTERVAL {
            self.flush();
        }
    }

    /// Write everything buffered. Best-effort: a failure disables the log
    /// rather than propagating into the reap path.
    pub fn flush(&mut self) {
        if self.disabled || self.buffered.is_empty() {
            return;
        }
        let body = format!("{}\n", self.buffered.join("\n"));
        if self.append(&body).is_err() {
            self.disabled = true;
        }
        self.buffered.clear();
        self.last_flush = Instant::now();
    }

    fn append(&self, body: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(body.as_bytes())
    }

    #[cfg(test)]
    fn buffered_len(&self) -> usize {
        self.buffered.len()
    }
}

impl Drop for ReapLog {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Where this session's reap log lives, alongside its other artifacts.
pub fn session_reap_log_path(
    state_dir: &Path,
    session_pid: u32,
    session_start_epoch: u64,
) -> PathBuf {
    crate::bridge_log::forensic_sessions_dir(state_dir)
        .join(format!("{session_pid}__{session_start_epoch}"))
        .join("reap.jsonl")
}

/// A fixed path makes the last known reaper pressure discoverable even when a
/// session ends in a system reset before its buffered JSONL log is flushed.
pub fn session_reap_health_path(
    state_dir: &Path,
    session_pid: u32,
    session_start_epoch: u64,
) -> PathBuf {
    session_reap_log_path(state_dir, session_pid, session_start_epoch)
        .with_file_name("reap-health.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(action: ReapAction, phase: ReapPhase) -> ReapEvent {
        ReapEvent {
            ts_ms: 1,
            pid: Some(42),
            parent_pid: Some(7),
            start_time: Some(1_700_000_000),
            image_name: Some("node.exe".into()),
            action,
            reason: "leaked_tool_client",
            phase,
        }
    }

    /// Both identities must hold exactly over a long session. They are
    /// separate because the populations are disjoint, so a test that only
    /// checked one would miss the other drifting.
    #[test]
    fn both_reconciliation_identities_hold_over_a_thousand_process_session() {
        let mut counters = ReapCounters::default();
        for round in 0..1_000u64 {
            counters.tracked += 1;
            // One in ten spawned processes is a shell that becomes a trigger.
            if round % 10 == 0 {
                counters.shell_exits_observed += 1;
                if round % 100 == 0 {
                    counters.abandoned += 1;
                } else {
                    counters.finalized += 1;
                    counters.decisions_emitted += 1;
                    if round % 20 == 0 {
                        counters.reaped_runtime += 1;
                    } else {
                        counters.spared += 1;
                    }
                }
            }
        }
        // ...and a few triggers never resolved at all.
        counters.shell_exits_observed += 3;

        assert!(counters.identities_hold());
        assert_eq!(counters.still_pending(), 3);
        assert_eq!(
            counters.finalized + counters.abandoned + counters.still_pending(),
            counters.shell_exits_observed
        );
        assert_eq!(
            counters.reaped() + counters.spared,
            counters.decisions_emitted
        );
    }

    /// `tracked` is a standalone denominator: it is *not* the sum of anything
    /// in either identity, because most spawned processes never become reap
    /// candidates at all.
    #[test]
    fn tracked_is_not_part_of_either_identity() {
        let counters = ReapCounters {
            tracked: 1_284,
            shell_exits_observed: 312,
            finalized: 308,
            abandoned: 4,
            decisions_emitted: 312,
            reaped_runtime: 36,
            reaped_at_exit: 4,
            spared: 272,
            ..ReapCounters::default()
        };
        assert!(counters.identities_hold());
        assert_eq!(counters.still_pending(), 0);
        assert_eq!(counters.reaped(), 40);
    }

    // ---- #689: metadata misses are summarized, not enumerated ----

    /// The diagnostic must survive being moved off the shared daemon log. A
    /// session that saw nothing *but* misses still prints, and prints the
    /// count — otherwise #689 deletes the signal instead of relocating it.
    #[test]
    fn a_session_of_only_metadata_misses_still_reports_them() {
        let counters = ReapCounters {
            metadata_misses: 11_441,
            ..ReapCounters::default()
        };
        assert!(
            !counters.is_silent(),
            "misses alone must not be mistaken for an idle session"
        );
        let lines = counters.summary_lines(None).expect("summary");
        assert!(
            lines.iter().any(|line| line.contains("11441")),
            "the per-session count is the whole point of the aggregate: {lines:?}"
        );
        assert!(counters
            .measurement_line()
            .contains("metadata_misses=11441"));
    }

    /// Misses do not belong to either reconciliation identity: an unresolvable
    /// process never became a reap candidate, so it emitted no decision.
    #[test]
    fn metadata_misses_are_outside_both_reconciliation_identities() {
        let counters = ReapCounters {
            shell_exits_observed: 10,
            finalized: 10,
            decisions_emitted: 4,
            reaped_runtime: 1,
            spared: 3,
            metadata_misses: 900,
            ..ReapCounters::default()
        };
        assert!(counters.identities_hold());
    }

    /// `ACTIVE_PROCESS_ZERO` clears the tracker's accounting mid-session, so
    /// per-epoch counts must be folded into session totals rather than lost.
    #[test]
    fn epoch_counters_accumulate_into_session_totals() {
        let mut session = ReapCounters {
            tracked: 10,
            reaped_runtime: 1,
            peak_known: 50,
            metadata_misses: 7,
            ..ReapCounters::default()
        };
        session.absorb(&ReapCounters {
            tracked: 5,
            reaped_runtime: 2,
            peak_known: 30,
            metadata_misses: 4,
            ..ReapCounters::default()
        });

        assert_eq!(session.tracked, 15);
        assert_eq!(session.reaped_runtime, 3);
        assert_eq!(session.metadata_misses, 11);
        assert_eq!(
            session.peak_known, 50,
            "a high-water mark is a max, not a sum"
        );
    }

    /// #706: `host_scans` is a rate to be driven down, so it sums; the batch
    /// size is a high-water mark, so it maxes.
    #[test]
    fn host_scans_sum_and_peak_batch_is_a_high_water_mark() {
        let mut session = ReapCounters {
            host_scans: 12,
            peak_batch: 37,
            ..ReapCounters::default()
        };
        session.absorb(&ReapCounters {
            host_scans: 5,
            peak_batch: 9,
            ..ReapCounters::default()
        });

        assert_eq!(session.host_scans, 17);
        assert_eq!(
            session.peak_batch, 37,
            "a high-water mark is a max, not a sum"
        );
    }

    /// The whole point of #706 is being able to read the ratio back out: a
    /// healthy session does far fewer host enumerations than it handles
    /// completion-port messages.
    ///
    /// Note `host_scans` is *not* bounded by `ticks` — it counts every
    /// `snapshot()` call site, and one quiet-period tick can drive both a
    /// retry scan and a batch scan.
    #[test]
    fn the_measurement_line_exposes_host_scans_and_peak_batch() {
        let counters = ReapCounters {
            ticks: 900,
            host_scans: 120,
            peak_batch: 214,
            ..ReapCounters::default()
        };
        let line = counters.measurement_line();
        assert!(line.contains("host_scans=120"), "{line}");
        assert!(line.contains("peak_batch=214"), "{line}");
        assert!(line.contains("host_scans_deferred=0"), "{line}");
    }

    /// A session that tracked nothing prints nothing.
    #[test]
    fn the_summary_is_suppressed_when_nothing_was_tracked() {
        assert!(ReapCounters::default().summary_lines(None).is_none());
        assert!(ReapCounters::default().is_silent());
    }

    #[test]
    fn the_summary_reports_both_identities_and_the_log_path() {
        let counters = ReapCounters {
            tracked: 1_284,
            shell_exits_observed: 312,
            finalized: 308,
            abandoned: 4,
            decisions_emitted: 312,
            reaped_runtime: 36,
            reaped_at_exit: 4,
            spared: 272,
            ..ReapCounters::default()
        };
        let lines = counters
            .summary_lines(Some(Path::new(
                "C:\\u\\.clud\\state\\sessions\\1__2\\reap.jsonl",
            )))
            .expect("a session that tracked work must report it");

        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("1284 tracked"));
        assert!(lines[0].contains("312 shell exits"));
        assert!(lines[0].contains("308 finalized, 4 abandoned"));
        assert!(lines[1].contains("40 reaped (36 runtime + 4 at exit)"));
        assert!(lines[1].contains("272 spared"));
        assert!(lines[2].contains("reap.jsonl"));
    }

    /// Peaks are what show growth tracking session age; they must be observed,
    /// not derived.
    #[test]
    fn size_observations_record_high_water_marks() {
        let mut counters = ReapCounters::default();
        counters.observe_sizes(10, 2);
        counters.observe_sizes(4, 7);
        assert_eq!(counters.peak_known, 10);
        assert_eq!(counters.peak_backlog, 7);
    }

    /// The writer buffers rather than flushing per event -- #544 found
    /// per-operation synchronous JSONL flushes to be an idle-CPU cost in
    /// their own right.
    #[test]
    fn events_are_buffered_and_flushed_in_batches() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
        let path = dir.path().join("sessions").join("1__2").join("reap.jsonl");
        let mut log = ReapLog::new(path.clone());

        log.record(&event(ReapAction::Reaped, ReapPhase::Runtime));
        assert_eq!(log.buffered_len(), 1);
        assert!(!path.exists(), "one event must not have touched the disk");

        log.flush();
        assert_eq!(log.buffered_len(), 0);
        let body = fs::read_to_string(&path).expect("flushed log");
        assert_eq!(body.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["action"], "reaped");
        assert_eq!(parsed["phase"], "runtime");
        assert_eq!(parsed["reason"], "leaked_tool_client");
        assert_eq!(parsed["pid"], 42);
        assert_eq!(parsed["parent_pid"], 7);
        assert_eq!(parsed["image_name"], "node.exe");
    }

    #[test]
    fn a_full_buffer_flushes_itself() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
        let path = dir.path().join("reap.jsonl");
        let mut log = ReapLog::new(path.clone());

        for _ in 0..FLUSH_LINES {
            log.record(&event(ReapAction::Spared, ReapPhase::Exit));
        }
        assert_eq!(log.buffered_len(), 0);
        assert_eq!(
            fs::read_to_string(&path).unwrap().lines().count(),
            FLUSH_LINES
        );
    }

    /// A no-op pass writes nothing at all: an empty flush must not create the
    /// file, or every idle session would leave a stray artifact behind.
    #[test]
    fn a_pass_that_changed_nothing_writes_nothing() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
        let path = dir.path().join("reap.jsonl");
        let mut log = ReapLog::new(path.clone());

        log.flush();
        log.flush();
        assert!(!path.exists());
    }

    /// A log that cannot be written degrades to a no-op rather than retrying
    /// on every event from inside the reap path.
    #[test]
    fn an_unwritable_log_disables_itself_instead_of_retrying() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
        // A path whose parent is an existing *file* can never be created.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        let mut log = ReapLog::new(blocker.join("reap.jsonl"));

        log.record(&event(ReapAction::Reaped, ReapPhase::Runtime));
        log.flush();
        assert!(log.disabled);

        log.record(&event(ReapAction::Reaped, ReapPhase::Runtime));
        assert_eq!(log.buffered_len(), 0, "a disabled log must not buffer");
    }

    #[test]
    fn unit_test_reap_log_is_isolated_from_production_sessions() {
        let path = session_reap_log_path(Path::new("/state"), 47180, 1_700_000_000);
        assert!(path.ends_with("test-sessions/47180__1700000000/reap.jsonl"));
    }

    #[test]
    fn flight_recorder_needs_no_clock_history_and_still_throttles() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
        let path = dir.path().join("reap-health.json");
        let mut recorder = ReapFlightRecorder::new(path.clone());
        assert!(
            recorder.last_write.is_none(),
            "construction must not do Instant arithmetic"
        );
        recorder.checkpoint(&ReapCounters::default());
        assert!(path.exists(), "the first checkpoint writes immediately");
        fs::remove_file(&path).unwrap();
        recorder.checkpoint(&ReapCounters::default());
        assert!(
            !path.exists(),
            "a second checkpoint inside the interval is throttled"
        );
    }

    #[test]
    fn flight_recorder_persists_counters_for_a_crashed_session() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
        let path = dir.path().join("reap-health.json");
        let mut recorder = ReapFlightRecorder::new(path.clone());
        recorder.checkpoint(&ReapCounters {
            host_scans: 12,
            host_scans_deferred: 9,
            ..ReapCounters::default()
        });
        let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["counters"]["host_scans"], 12);
        assert_eq!(value["counters"]["host_scans_deferred"], 9);
    }
}

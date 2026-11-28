//! Publish the daemon's CPU-alert state to a file instead of answering M polls
//! for it (#547, sub-issue of the #542 idle-CPU meta).
//!
//! # What this replaces
//!
//! Every Windows `clud` client used to issue a `DaemonRequest::Metrics`
//! loopback round-trip every 2 seconds, forever, so it could flash the console
//! title when the daemon exceeds 70% CPU. With M open terminals that is M
//! connections every 2 s and M single-PID sysinfo refreshes daemon-side — and
//! in the idle steady state **100% of that traffic produces "no alert"**.
//!
//! # The inversion
//!
//! The daemon samples its own CPU on one timer and writes
//! [`METRICS_SNAPSHOT`] **only when the alert-relevant state changes**. Clients
//! `stat` that file's mtime once per keeper pass — no socket, no daemon work —
//! and parse it only when the mtime moved.
//!
//! Idle steady state: zero daemon writes, zero client connections, one `stat`
//! per client per tick.
//!
//! # Why a file rather than a pushed connection
//!
//! clud's IPC is request/response. Holding a persistent per-client connection
//! open for push would keep M sockets alive and complicate daemon shutdown. An
//! mtime `stat` is the cheapest cross-process signal available, and it degrades
//! the way the old code already did: the file being absent or unreadable clears
//! the alert, exactly like the previous `Err(_) => clear_cpu_alert()`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Alert threshold, in percent. Mirrors `console_title::CPU_FLASH_THRESHOLD_PCT`
/// — the client still owns the *presentation* decision; this is only about when
/// a change is worth publishing.
pub(super) const CPU_ALERT_THRESHOLD_PCT: f32 = 70.0;

/// While above the threshold, republish only once the reading has moved by this
/// much. Without it a daemon sitting at 71–99% would rewrite the file on every
/// tick, which is the polling cost again wearing a different hat.
pub(super) const CPU_ALERT_HYSTERESIS_PCT: f32 = 10.0;

/// How often the daemon samples its own CPU.
///
/// The same 2 s the clients used to poll at, so alert latency is unchanged —
/// but paid **once**, by one process, instead of once per open terminal.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// The value published once at daemon startup so the snapshot file exists
/// before the first alert-worthy reading (#706).
///
/// Zero rather than a real first sample: CPU percentage needs two samples to
/// mean anything, and a below-threshold value displays identically whatever it
/// is. If the daemon really is busy at startup, the first genuine sample two
/// seconds later crosses the threshold and republishes.
pub(super) const BASELINE_CPU_PCT: f32 = 0.0;

/// Snapshot file, beside the other state-dir sentinels.
pub(super) const METRICS_SNAPSHOT: &str = "metrics.json";

pub fn metrics_snapshot_path(state_dir: &Path) -> PathBuf {
    state_dir.join(METRICS_SNAPSHOT)
}

/// The published snapshot. Deliberately tiny: a client reads this on a UI tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub pid: u32,
    pub cpu_pct: f32,
    /// Wall-clock milliseconds since the epoch, for staleness checks by any
    /// consumer that wants them. The client's own freshness signal is the
    /// file's mtime, so this is diagnostic rather than load-bearing.
    pub ts_ms: u64,
}

/// Is `current` an alert-relevant change from `previous`?
///
/// Pure so the publish policy is unit-tested without a daemon, a clock or a
/// filesystem — the same discipline the reap decisions follow.
///
/// Publishes when:
/// - nothing has been published yet **and** we are above the threshold. A
///   below-threshold first sample writes nothing: absence already means "no
///   alert", so writing it would be pure cost.
/// - the reading crosses the threshold in either direction. The falling edge
///   matters as much as the rising one — it is what clears a stuck alert.
/// - both readings are above the threshold and they differ by at least
///   [`CPU_ALERT_HYSTERESIS_PCT`], so the displayed number does not go stale
///   while a load persists.
pub(super) fn should_publish(previous: Option<f32>, current: f32) -> bool {
    let above = |value: f32| value > CPU_ALERT_THRESHOLD_PCT;
    match previous {
        None => above(current),
        Some(previous) => {
            if above(previous) != above(current) {
                return true;
            }
            above(current) && (current - previous).abs() >= CPU_ALERT_HYSTERESIS_PCT
        }
    }
}

/// Sample the daemon's CPU on a timer and publish only on transitions.
pub(super) fn spawn_cpu_alert_publisher(
    state_dir: PathBuf,
    shutdown_requested: Arc<AtomicBool>,
    mut sample: impl FnMut() -> f32 + Send + 'static,
) {
    let _ = thread::Builder::new()
        .name("clud-cpu-publish".to_string())
        .spawn(move || {
            // #706: publish one baseline immediately, so the file always
            // exists from the moment the daemon is up.
            //
            // `should_publish(None, x)` is deliberately false for any `x`
            // below the threshold — writing "no alert" is pure cost. But
            // nothing else in the tree ever creates this file, so on a healthy
            // box that never spikes above 70% it was *never* created at all.
            // `classify_snapshot` then returned `Absent` on every client pass,
            // and every Windows client fell through to the 15 s
            // `DaemonRequest::Metrics` compatibility RPC — forever. The
            // "older daemon" path was the steady state on the current daemon,
            // which made #547 a 7.5× improvement rather than the elimination
            // it was meant to be.
            //
            // With the file present and unchanged, `classify_snapshot` returns
            // `Unchanged` and the idle cost is one `stat` per keeper pass and
            // zero connections, which is what #547 specified.
            let mut published: Option<f32> = None;
            if write_snapshot(&state_dir, BASELINE_CPU_PCT).is_ok() {
                published = Some(BASELINE_CPU_PCT);
            }
            loop {
                // Sleep in slices so shutdown is observed within ~250 ms
                // rather than at the end of a full interval.
                let mut remaining = SAMPLE_INTERVAL;
                while remaining > Duration::ZERO {
                    if shutdown_requested.load(Ordering::SeqCst) {
                        return;
                    }
                    let slice = remaining.min(Duration::from_millis(250));
                    thread::sleep(slice);
                    remaining = remaining.saturating_sub(slice);
                }
                if shutdown_requested.load(Ordering::SeqCst) {
                    return;
                }
                super::startup_phases::rotate_redirected_log(&state_dir);
                let cpu_pct = sample();
                if !should_publish(published, cpu_pct) {
                    continue;
                }
                if write_snapshot(&state_dir, cpu_pct).is_ok() {
                    published = Some(cpu_pct);
                }
            }
        });
}

fn write_snapshot(state_dir: &Path, cpu_pct: f32) -> std::io::Result<()> {
    let snapshot = MetricsSnapshot {
        pid: std::process::id(),
        cpu_pct,
        ts_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    };
    super::io_helpers::write_json_file(&metrics_snapshot_path(state_dir), &snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of #547: an idle daemon *re-writes* nothing, so an idle
    /// client pays one `stat` per pass and reads nothing.
    ///
    /// Note this is about the sampling loop only. Since #706 the publisher
    /// writes one baseline before the loop starts — see
    /// [`the_baseline_keeps_an_idle_daemon_from_forcing_clients_onto_the_rpc`]
    /// for why the file must exist even when there is no alert.
    #[test]
    fn an_idle_daemon_publishes_nothing() {
        assert!(!should_publish(None, 0.0));
        assert!(!should_publish(None, 12.5));
        assert!(!should_publish(None, CPU_ALERT_THRESHOLD_PCT));
    }

    /// #706: the baseline exists so `metrics.json` is present on a healthy
    /// box, and it must not turn into a write on every tick.
    ///
    /// Before the baseline, `should_publish(None, low)` being false meant the
    /// file was never created at all, `classify_snapshot` returned `Absent`
    /// forever, and every client fell back to the 15 s RPC — the compatibility
    /// path became the steady state.
    #[test]
    fn the_baseline_keeps_an_idle_daemon_from_forcing_clients_onto_the_rpc() {
        // Having published the baseline, an idle daemon stays silent: every
        // below-threshold reading compares against a below-threshold previous.
        assert!(!should_publish(Some(BASELINE_CPU_PCT), 0.0));
        assert!(!should_publish(Some(BASELINE_CPU_PCT), 12.5));
        assert!(!should_publish(
            Some(BASELINE_CPU_PCT),
            CPU_ALERT_THRESHOLD_PCT
        ));

        // ...but a real spike still publishes, so seeding the baseline cannot
        // swallow the first alert.
        assert!(should_publish(Some(BASELINE_CPU_PCT), 85.0));
    }

    /// The baseline must read back as "no alert", not as a missing field or a
    /// value the client would render as a spike.
    #[test]
    fn the_baseline_round_trips_as_a_below_threshold_reading() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
        write_snapshot(dir.path(), BASELINE_CPU_PCT).expect("write baseline");

        let text = std::fs::read_to_string(metrics_snapshot_path(dir.path()))
            .expect("baseline snapshot is readable");
        let snapshot: MetricsSnapshot =
            serde_json::from_str(&text).expect("baseline snapshot parses");

        assert_eq!(snapshot.cpu_pct, BASELINE_CPU_PCT);
        assert_eq!(snapshot.pid, std::process::id());
        assert!(
            snapshot.cpu_pct <= CPU_ALERT_THRESHOLD_PCT,
            "the baseline must never render as an alert"
        );
    }

    #[test]
    fn crossing_the_threshold_publishes_in_both_directions() {
        // Rising edge — the alert must appear.
        assert!(should_publish(Some(10.0), 85.0));
        assert!(should_publish(None, 85.0));
        // Falling edge — just as important: this is what clears a stuck alert.
        assert!(should_publish(Some(85.0), 10.0));
    }

    /// A daemon pinned at high CPU must not turn "publish on change" back into
    /// "publish every tick" — that would be the polling cost relocated, not
    /// removed.
    #[test]
    fn small_moves_above_the_threshold_do_not_republish() {
        assert!(!should_publish(Some(85.0), 86.0));
        assert!(!should_publish(Some(85.0), 80.0));
        // ...but a big move does, so a displayed number does not go stale.
        assert!(should_publish(Some(85.0), 95.0));
        assert!(should_publish(Some(95.0), 85.0));
    }

    /// Quiet churn below the threshold is the common case on a busy dev box and
    /// must stay silent — it produces no alert either way.
    #[test]
    fn churn_below_the_threshold_never_publishes() {
        let mut published = None;
        for reading in [0.0_f32, 5.0, 40.0, 3.0, 69.9, 0.0] {
            assert!(
                !should_publish(published, reading),
                "below-threshold reading {reading} triggered a write"
            );
            if should_publish(published, reading) {
                published = Some(reading);
            }
        }
    }

    /// Walk a realistic load spike and count the writes. The assertion is the
    /// acceptance criterion "snapshot writes occur only on state transitions",
    /// expressed as a number rather than a vibe.
    #[test]
    fn a_load_spike_costs_exactly_two_writes() {
        let readings = [
            1.0_f32, 2.0, 0.5, 3.0, // idle: no writes
            90.0, 91.0, 89.0, 92.0, // spike: one write on the rising edge
            2.0, 1.0, // recovery: one write on the falling edge
        ];
        let mut published: Option<f32> = None;
        let mut writes = 0;
        for reading in readings {
            if should_publish(published, reading) {
                published = Some(reading);
                writes += 1;
            }
        }
        assert_eq!(writes, 2, "expected one rising and one falling edge only");
    }

    #[test]
    fn a_written_snapshot_round_trips() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
        write_snapshot(dir.path(), 88.5).expect("write snapshot");
        let text = std::fs::read_to_string(metrics_snapshot_path(dir.path())).expect("read");
        let snapshot: MetricsSnapshot = serde_json::from_str(&text).expect("parse");
        assert_eq!(snapshot.pid, std::process::id());
        assert!((snapshot.cpu_pct - 88.5).abs() < f32::EPSILON);
    }
}

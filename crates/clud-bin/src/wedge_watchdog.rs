//! Issue #541: wedge watchdog for a monitored backend child process.
//!
//! ## The incident
//!
//! A live `clud --codex` session (2026-07-22, Windows 10, WT 1.24) showed
//! `codex.exe`'s TUI frozen while the agent kept working underneath. Live
//! diagnosis found: 37 threads total, one thread permanently `Running` and
//! consuming 46.5 of the process's 48.6 CPU-minutes (97% user-mode — a pure
//! busy-loop, not a stuck syscall), while process IO read/write bytes sat at
//! 0 B/s for the whole ~48 minute episode. Root cause is upstream
//! (openai/codex#33755, a `syntect` highlighting livelock; fixed on codex's
//! main), but `clud` owns the UX, launched the process, and gave the user no
//! signal that tokens were burning against a dead UI.
//!
//! ## Detection signature
//!
//! Over [`DEFAULT_REQUIRED_STREAK`] consecutive [`DEFAULT_TICK`]-spaced
//! observation windows (9 × 10 s ≈ 90 s, inside the 60-120 s acceptance
//! band), the monitored child's process subtree must show **both**:
//!
//! - (a) a single thread consuming ≥ [`DEFAULT_USER_PCT_THRESHOLD`] (90 %)
//!   of one core in user-mode time, and
//! - (b) subtree IO write-bytes delta ≤ [`DEFAULT_IO_EPSILON_BYTES`] (a
//!   small epsilon, not a strict zero, so incidental heartbeat writes don't
//!   mask a real wedge).
//!
//! Either condition failing resets the streak — this rules out both
//! "busy but healthy" processes (high CPU *with* output) and spread
//! multi-thread compute (no single thread dominates).
//!
//! ## Architecture
//!
//! - [`WedgeDetector`] is the pure, platform-free decision core: feed it
//!   [`Sample`] values (wall-clock deltas the caller already measured) and
//!   read back a [`WedgeState`]. No I/O, no OS calls — exhaustively unit
//!   tested below without needing a real spinning process.
//! - `platform/windows/wedge_watchdog.rs` is the platform sampler: it walks
//!   the process subtree rooted at the monitored pid via
//!   `CreateToolhelp32Snapshot`, finds the single hottest thread via
//!   `OpenThread` + `GetThreadTimes`, and reads that thread's owning
//!   process's IO write bytes via `OpenProcess` + `GetProcessIoCounters`.
//! - [`WedgeWatchdog`] is the background-thread wiring (same shape as
//!   [`crate::cpu_banner::BannerWatcher`]): spawns a sampler + detector loop,
//!   prints one rate-limited warning per wedge episode, and logs the
//!   measured signature via [`crate::verbose_log`].
//! - Non-Windows builds compile [`WedgeWatchdog::spawn`] to a no-op (no
//!   thread spawned) — detection is unavailable there today, so the CI
//!   matrix still passes cleanly.

use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

/// Tick cadence for the background sampler. 9 ticks × 10 s ≈ 90 s total,
/// inside the issue's 60-120 s acceptance band for detection latency.
pub const DEFAULT_TICK: Duration = Duration::from_secs(10);

/// Consecutive qualifying windows required before [`WedgeDetector`] reports
/// [`WedgeState::Wedged`]. `DEFAULT_TICK * DEFAULT_REQUIRED_STREAK` = 90 s.
pub const DEFAULT_REQUIRED_STREAK: u32 = 9;

/// Fraction of one core's user-mode time a single thread must sustain to
/// count as "hot". The live incident measured 97% user-mode on the pinned
/// thread; 90% leaves headroom for sampling jitter while still requiring a
/// genuine single-thread pin (a well-behaved TUI's render thread wakes,
/// draws, and sleeps — it doesn't sit at 90%+ of one core).
pub const DEFAULT_USER_PCT_THRESHOLD: f64 = 0.90;

/// Small epsilon for "no console output", not a strict zero, so incidental
/// heartbeat/log writes from an otherwise-dead TUI don't mask a real wedge.
/// The live incident measured exactly 0 B/s; 4 KiB over a 10 s window is
/// generous slack in the other direction.
pub const DEFAULT_IO_EPSILON_BYTES: u64 = 4096;

/// Process-level user-time fraction that arms the expensive per-thread walk
/// (#709).
///
/// Deliberately *below* [`DEFAULT_USER_PCT_THRESHOLD`]: a thread's user time
/// can never exceed its process's, so any value at or above the detector's own
/// threshold would already be sound — the slack exists so the gate can never be
/// the component that decides a borderline case. It only ever answers "is the
/// expensive measurement worth taking".
///
/// Consumed by the Windows-only sampler, and by the Tier-1 tests on every
/// platform.
pub const GATE_USER_PCT_THRESHOLD: f64 = 0.80;

/// The gate must arm *below* the detector's threshold, or it could skip the
/// measurement for a subtree that is about to qualify as wedged. Enforced at
/// compile time rather than by a test: it is a relationship between two
/// constants, so a build failure is the honest signal.
const _: () = assert!(GATE_USER_PCT_THRESHOLD < DEFAULT_USER_PCT_THRESHOLD);

// ─── Pure cores for the two #709 changes ───────────────────────────────
//
// Both live here rather than in the Windows sampler so they are asserted on
// every platform CI builds for, the same discipline `WedgeDetector` follows.

/// Could any single thread in the subtree be at the wedge threshold?
///
/// Compares each process's *user-mode* delta against `gate_pct * wall`. A
/// thread's user time can never exceed its process's, so a `false` here proves
/// no thread qualifies and the caller may skip the host-wide thread walk.
///
/// **Fails open.** Returns `true` whenever the question cannot be answered —
/// no wall delta, no baseline, an empty reading, or a pid with no previous
/// sample. This gate exists to save work on healthy sessions, never to decide
/// that a session is healthy.
///
/// Only the Windows sampler calls this in production, but the decision is
/// platform-free so the tests exercise it everywhere.
pub fn subtree_could_hide_a_hot_thread(
    prev_process_user_100ns: &std::collections::HashMap<u32, u64>,
    cur_process_user_100ns: &std::collections::HashMap<u32, u64>,
    wall_delta: Option<Duration>,
    gate_pct: f64,
) -> bool {
    let Some(wall) = wall_delta else {
        return true;
    };
    if prev_process_user_100ns.is_empty() || cur_process_user_100ns.is_empty() {
        return true;
    }
    let wall_100ns = (wall.as_nanos() / 100) as u64;
    if wall_100ns == 0 {
        return true;
    }
    let need = ((wall_100ns as f64) * gate_pct) as u64;
    cur_process_user_100ns.iter().any(|(pid, user)| {
        prev_process_user_100ns
            .get(pid)
            // A pid with no baseline is new this tick; we cannot rule it out.
            .is_none_or(|prev| user.saturating_sub(*prev) >= need)
    })
}

/// Every descendant of `root` in a parent→children adjacency map, including
/// `root` itself.
///
/// **The visited set is load-bearing, not tidiness.** Windows recycles PIDs, so
/// a Toolhelp parent-pid graph can contain a cycle — a recycled number becoming
/// its own ancestor. Without it this walk never terminates and the output grows
/// without bound (#709).
///
/// Only the Windows sampler calls this in production; the walk itself is
/// platform-free so the cycle cases are asserted everywhere.
pub fn descendants_of(children: &std::collections::HashMap<u32, Vec<u32>>, root: u32) -> Vec<u32> {
    let mut seen = std::collections::HashSet::from([root]);
    let mut stack = vec![root];
    let mut out = vec![root];
    while let Some(cur) = stack.pop() {
        let Some(kids) = children.get(&cur) else {
            continue;
        };
        for &kid in kids {
            if seen.insert(kid) {
                out.push(kid);
                stack.push(kid);
            }
        }
    }
    out
}

// ─── Pure decision core ────────────────────────────────────────────────

/// One observation window's measurement, fed into [`WedgeDetector::observe`].
/// Platform-free: the caller (production sampler or a test) has already done
/// all the OS-specific work and reduced it to three deltas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// User-mode CPU time consumed by the single hottest thread in the
    /// monitored subtree during this window.
    pub hottest_thread_user_delta: Duration,
    /// Wall-clock time elapsed since the previous sample.
    pub wall_delta: Duration,
    /// IO write-bytes delta (`IO_COUNTERS::WriteTransferCount` on Windows)
    /// for the process that owns the hottest thread, during this window.
    pub io_write_delta: u64,
}

/// Detector state. `Suspect` carries the current streak so callers/tests can
/// observe progress toward `Wedged` without waiting for the full window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WedgeState {
    Healthy,
    Suspect { streak: u32 },
    Wedged,
}

/// Tunable thresholds for [`WedgeDetector`]. See the `DEFAULT_*` constants
/// for rationale; callers (tests, or a future settings.json knob) can
/// override any of them.
#[derive(Debug, Clone, Copy)]
pub struct WedgeDetectorCfg {
    pub user_pct_threshold: f64,
    pub io_epsilon_bytes: u64,
    pub required_streak: u32,
}

impl Default for WedgeDetectorCfg {
    fn default() -> Self {
        Self {
            user_pct_threshold: DEFAULT_USER_PCT_THRESHOLD,
            io_epsilon_bytes: DEFAULT_IO_EPSILON_BYTES,
            required_streak: DEFAULT_REQUIRED_STREAK,
        }
    }
}

/// Pure state machine. No I/O, no OS calls — drive it from any sampler, or
/// directly from a test.
#[derive(Debug)]
pub struct WedgeDetector {
    cfg: WedgeDetectorCfg,
    streak: u32,
    streak_wall: Duration,
    state: WedgeState,
}

impl WedgeDetector {
    pub fn new(cfg: WedgeDetectorCfg) -> Self {
        Self {
            cfg,
            streak: 0,
            streak_wall: Duration::ZERO,
            state: WedgeState::Healthy,
        }
    }

    /// Feed one window's measurement. Both the single-thread user% and the
    /// io-quiet conditions must hold for the window to count toward the
    /// streak; either failing resets it immediately back to `Healthy`
    /// (covers both "brief spike then idle" and "recovery after Wedged").
    pub fn observe(&mut self, sample: Sample) -> WedgeState {
        let hot = user_pct(&sample) >= self.cfg.user_pct_threshold;
        let quiet = sample.io_write_delta <= self.cfg.io_epsilon_bytes;

        if hot && quiet {
            self.streak = self.streak.saturating_add(1);
            self.streak_wall += sample.wall_delta;
            self.state = if self.streak >= self.cfg.required_streak {
                WedgeState::Wedged
            } else {
                WedgeState::Suspect {
                    streak: self.streak,
                }
            };
        } else {
            self.streak = 0;
            self.streak_wall = Duration::ZERO;
            self.state = WedgeState::Healthy;
        }
        self.state
    }

    pub fn state(&self) -> WedgeState {
        self.state
    }

    /// Total wall-clock span of the current qualifying streak. Used to
    /// render "no console output for Xs" in the warning line.
    pub fn streak_wall(&self) -> Duration {
        self.streak_wall
    }
}

fn user_pct(sample: &Sample) -> f64 {
    if sample.wall_delta.is_zero() {
        return 0.0;
    }
    sample.hottest_thread_user_delta.as_secs_f64() / sample.wall_delta.as_secs_f64()
}

// ─── Watchdog wiring ────────────────────────────────────────────────────

/// Caller-built configuration for [`WedgeWatchdog::spawn`]. Mirrors the
/// shape of [`crate::cpu_banner::CpuBannerCfg`].
#[derive(Debug, Clone)]
pub struct WedgeWatchdogCfg {
    pub enabled: bool,
    /// PID of the monitored child (the direct child clud spawned — the
    /// sampler walks its full descendant subtree, since the actual TUI
    /// binary is often several process hops below, e.g. cmd -> node ->
    /// node -> codex.exe in the live incident).
    pub pid: u32,
    /// Backend executable name, used in the warning line
    /// (`"codex"` / `"claude"`).
    pub backend_label: String,
    pub tick: Duration,
    pub required_streak: u32,
    pub user_pct_threshold: f64,
    pub io_epsilon_bytes: u64,
    /// Test-only hook: when set, every observed [`WedgeState`] transition
    /// is also sent here so tests can assert on the real background loop
    /// without scraping stderr. `None` in production.
    pub state_tx: Option<mpsc::Sender<WedgeState>>,
}

impl WedgeWatchdogCfg {
    pub fn new(pid: u32, backend_label: impl Into<String>) -> Self {
        Self {
            enabled: true,
            pid,
            backend_label: backend_label.into(),
            tick: DEFAULT_TICK,
            required_streak: DEFAULT_REQUIRED_STREAK,
            user_pct_threshold: DEFAULT_USER_PCT_THRESHOLD,
            io_epsilon_bytes: DEFAULT_IO_EPSILON_BYTES,
            state_tx: None,
        }
    }

    /// Disabled variant — [`WedgeWatchdog::spawn`] is a no-op (no thread).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            pid: 0,
            backend_label: String::new(),
            tick: DEFAULT_TICK,
            required_streak: DEFAULT_REQUIRED_STREAK,
            user_pct_threshold: DEFAULT_USER_PCT_THRESHOLD,
            io_epsilon_bytes: DEFAULT_IO_EPSILON_BYTES,
            state_tx: None,
        }
    }

    /// Only reached from the watchdog loop, which runs on Windows alone.
    fn detector_cfg(&self) -> WedgeDetectorCfg {
        WedgeDetectorCfg {
            user_pct_threshold: self.user_pct_threshold,
            io_epsilon_bytes: self.io_epsilon_bytes,
            required_streak: self.required_streak,
        }
    }
}

/// Background watcher thread. Joins on `Drop`, same contract as
/// [`crate::cpu_banner::BannerWatcher`] — construct it right after starting
/// the monitored child and let it fall out of scope (or call `stop()`
/// explicitly) when that child's iteration ends, so each process gets a
/// fresh baseline.
pub struct WedgeWatchdog {
    stop_tx: Option<mpsc::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl WedgeWatchdog {
    /// Spawn the watcher. Inert (no thread) when `cfg.enabled` is false or
    /// on a platform without a sampler implementation.
    pub fn spawn(cfg: WedgeWatchdogCfg) -> Self {
        if !cfg.enabled || !platform_supported() {
            return Self {
                stop_tx: None,
                handle: None,
            };
        }
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("clud-wedge-watchdog".into())
            .spawn(move || run_watchdog_loop(cfg, rx))
            .ok();
        Self {
            stop_tx: Some(tx),
            handle,
        }
    }

    /// Convenience for callers holding `Option<u32>` (e.g.
    /// `NativeProcess::pid()` / `NativePtyProcess::pid()`, which can be
    /// `None` in edge cases). `None` spawns an inert watchdog.
    pub fn spawn_for_pid(pid: Option<u32>, backend_label: impl Into<String>) -> Self {
        match pid {
            Some(pid) => Self::spawn(WedgeWatchdogCfg::new(pid, backend_label)),
            None => Self::spawn(WedgeWatchdogCfg::disabled()),
        }
    }

    /// Explicit shutdown. Idempotent; safe to call before `Drop`. Returns
    /// promptly even mid-tick — the loop blocks on `recv_timeout(tick)`,
    /// which wakes immediately once the stop signal arrives.
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WedgeWatchdog {
    fn drop(&mut self) {
        self.stop();
    }
}

fn platform_supported() -> bool {
    crate::platform::host::WINDOWS
}

/// Only spawned where [`platform_supported`] holds (Windows).
fn run_watchdog_loop(cfg: WedgeWatchdogCfg, stop_rx: mpsc::Receiver<()>) {
    let mut sampler = crate::platform::wedge_watchdog::ThreadIoSampler::new();
    let mut detector = WedgeDetector::new(cfg.detector_cfg());
    let mut warned_this_episode = false;

    loop {
        if stop_rx.recv_timeout(cfg.tick).is_ok() {
            return;
        }
        let Some(tick) = sampler.tick(cfg.pid) else {
            continue;
        };
        let state = detector.observe(tick.sample);
        if let Some(tx) = &cfg.state_tx {
            let _ = tx.send(state);
        }
        match state {
            WedgeState::Wedged => {
                if !warned_this_episode {
                    warned_this_episode = true;
                    emit_wedge_warning(&cfg, &tick, detector.streak_wall());
                }
            }
            WedgeState::Healthy => {
                // Rate-limit: only the next Wedged transition after a
                // recovery warns again (one warning per wedge episode).
                warned_this_episode = false;
            }
            WedgeState::Suspect { .. } => {}
        }
    }
}

/// Print the user-visible warning and log the measured signature. Split out
/// from the loop so the message format is easy to eyeball/test in isolation.
fn emit_wedge_warning(
    cfg: &WedgeWatchdogCfg,
    tick: &crate::platform::wedge_watchdog::SampledTick,
    streak_wall: Duration,
) {
    let pct = user_pct(&tick.sample) * 100.0;
    let secs = streak_wall.as_secs();
    let backend = &cfg.backend_label;
    crate::loop_console::eprint_diagnostic(&format!(
        "clud: WARNING — {backend} TUI appears wedged (1 thread at {pct:.0}% CPU, no console \
         output for {secs}s). The session may still be working underneath. Consider restarting \
         the TUI: Ctrl+C, then `clud -c` (or `clud --resume`) to reconnect the {backend} session."
    ));
    crate::verbose_log::log(format_args!(
        "[clud] wedge-watchdog: WEDGED root_pid={root_pid} hot_pid={hot_pid} hot_tid={hot_tid} \
         user_pct={pct:.1} io_write_delta_bytes={io} streak_secs={secs} tick={tick_ms}ms",
        root_pid = cfg.pid,
        hot_pid = tick.hot_pid,
        hot_tid = tick.hot_tid,
        io = tick.sample.io_write_delta,
        tick_ms = cfg.tick.as_millis(),
    ));
}

// The Windows sampler lives in `platform/windows/wedge_watchdog.rs`.

/// #709 — the two pure cores: the cheap gate, and the cycle-safe walk.
#[cfg(test)]
#[path = "wedge_watchdog_gate_tests.rs"]
mod gate_tests;
#[cfg(test)]
#[path = "wedge_watchdog_tests.rs"]
mod tests;

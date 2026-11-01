//! Set the console window title to `clud <cwd-name>` on launch and keep
//! it pinned for the lifetime of the process.
//!
//! On Windows, when `clud` runs in cmd.exe / Windows Terminal, the title
//! bar otherwise shows the host shell's title — usually a generic
//! `Command Prompt` or the path to cmd.exe. Stamping `clud <cwd-name>`
//! makes it obvious at a glance which window is the active session and
//! which directory it's working in.
//!
//! Just stamping once isn't enough: clud spawns a TUI backend
//! (claude.exe / codex.exe) that — along with any tool subprocess it
//! invokes (git, npm, build runners) — emits OSC 0/2 title-set escape
//! sequences continuously. Two complementary defenses keep our title
//! visible:
//!
//! 1. [`keep_setting_in_background`] starts a low-frequency poller that
//!    re-applies our title whenever the live console title drifts. This
//!    is the only option in subprocess mode (the default Claude path on
//!    Windows) because the child inherits clud's stdio handles directly
//!    — we can't intercept its OSC bytes.
//!
//! 2. [`OscTitleStripper`] is a stream-resumable byte filter used by the
//!    PTY pump (`session.rs`) and the daemon attach relay
//!    (`daemon/attach.rs`, #1372) to drop OSC 0/2 sequences from the
//!    child's output before they reach our terminal. PTY mode is opt-in
//!    (`--pty`) and used by `clud loop` on POSIX. With the stripper in
//!    place the title doesn't flicker — the keeper rarely fires.
//!
//! The Win32 calls and the keeper thread live in
//! `platform/windows/console_title.rs`. On POSIX the stamp is a one-shot
//! OSC 2 on stdout (`platform/unix/console_title.rs`); without it, shells
//! with title integration (kitty) leave the window titled with the launch
//! command line (`clud -c`), indistinguishable across repos. The call sites
//! in `main.rs` don't need a `cfg`. The OSC stripper is platform-agnostic because
//! the PTY pump runs on every platform.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use std::time::Instant;

#[path = "console_title_osc.rs"]
mod console_title_osc;

pub const CPU_FLASH_THRESHOLD_PCT: f32 = 70.0;
/// Fallback RPC cadence, used only when the daemon publishes no snapshot file
/// (an older daemon, or one that has not sampled yet).
///
/// #547 dropped this from 2 s to 15 s: it is now a compatibility path, not the
/// steady state, and 2 s of polling per open terminal is the cost the issue
/// exists to remove. The snapshot path costs one `stat` per keeper pass.
pub const CPU_METRICS_FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(15);
pub const CPU_ALERT_TTL: Duration = Duration::from_millis(2500);
pub const CPU_FLASH_INTERVAL: Duration = Duration::from_millis(500);

/// Keeper cadence while anything is changing — unchanged from before
/// (issue #547): drift is corrected within a noticeable beat, and the flash
/// animation needs this resolution to look like a flash.
pub const KEEPER_FAST_INTERVAL: Duration = Duration::from_millis(750);

/// Ceiling the keeper backs off to once nothing has changed for a while.
///
/// Chosen against the drift it must correct: a title clobbered by a child
/// process is a cosmetic fault, and correcting it within 3 s of a quiet
/// terminal is unnoticeable, while 4× fewer wakeups per open terminal is not.
pub const KEEPER_IDLE_INTERVAL: Duration = Duration::from_millis(3_000);

/// Consecutive unchanged passes before backing off one step.
///
/// Four passes ≈ 3 s of genuine quiet, so a terminal being actively typed in
/// never reaches the slow cadence.
pub const KEEPER_STABLE_PASSES_BEFORE_BACKOFF: u32 = 4;

/// Backoff state for the title keeper (issue #547).
///
/// Split out as a pure state machine — like `update_cpu_alert_locked` — so the
/// cadence policy is testable without a console, a thread, or 3 s of waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeeperCadence {
    interval: Duration,
    stable_passes: u32,
}

impl KeeperCadence {
    pub fn new() -> Self {
        Self {
            interval: KEEPER_FAST_INTERVAL,
            stable_passes: 0,
        }
    }

    pub fn interval(self) -> Duration {
        self.interval
    }

    /// Record one keeper pass and return the cadence for the next sleep.
    ///
    /// `changed` means anything the keeper exists to react to moved: the title
    /// drifted and was re-stamped, or the CPU alert changed state.
    ///
    /// A change snaps straight back to the fast cadence rather than stepping
    /// down gradually. Responsiveness is the feature here; the backoff only
    /// exists to make *doing nothing* cheap, so it must never be the reason a
    /// visible change is slow to appear.
    pub fn record_pass(self, changed: bool) -> Self {
        if changed {
            return Self::new();
        }
        let stable_passes = self.stable_passes.saturating_add(1);
        if stable_passes < KEEPER_STABLE_PASSES_BEFORE_BACKOFF {
            return Self {
                interval: self.interval,
                stable_passes,
            };
        }
        // Doubling rather than jumping to the ceiling: a terminal that goes
        // quiet for a few seconds and then changes again pays only a small
        // latency penalty, while one quiet for minutes still reaches 3 s.
        let doubled = self.interval.saturating_mul(2).min(KEEPER_IDLE_INTERVAL);
        Self {
            interval: doubled,
            stable_passes: 0,
        }
    }
}

impl Default for KeeperCadence {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct TitleState {
    pub base_title: String,
    pub cpu_alert: Option<CpuAlert>,
}

#[derive(Debug)]
pub struct CpuAlert {
    pub title: String,
    pub observed_at: Instant,
    pub expires_at: Instant,
}

/// The title we want the console to display, shared between the
/// foreground thread and the keeper thread. Empty string means
/// `set_for_current_cwd` was never called and the keeper should idle.
pub fn title_state_cell() -> &'static Arc<Mutex<TitleState>> {
    static CELL: OnceLock<Arc<Mutex<TitleState>>> = OnceLock::new();
    CELL.get_or_init(|| {
        Arc::new(Mutex::new(TitleState {
            base_title: String::new(),
            cpu_alert: None,
        }))
    })
}

/// Set the console title to `clud <cwd-basename>` for the current
/// working directory and record it so `keep_setting_in_background` can
/// re-apply it after drift. Best-effort — failures are silent.
///
/// Called once near the top of `main`.
pub fn set_for_current_cwd() {
    let basename = current_cwd_basename().unwrap_or_else(|| "?".to_string());
    let title = title_for_cwd_name(&basename);
    let mut state = title_state_cell()
        .lock()
        .expect("title state mutex poisoned");
    state.base_title = title.clone();
    state.cpu_alert = None;
    drop(state);
    crate::platform::console_title::set_title(&title);
}

/// Spawn a Windows-only daemon thread that re-applies the desired title
/// every ~750 ms whenever the live console title has drifted away. No-op
/// if `set_for_current_cwd` was never called (desired title is empty).
///
/// Idempotent: the `OnceLock` guarantees at most one keeper thread per
/// process, even if this is called more than once.
///
/// The thread is a daemon — it has no join handle and runs until
/// process exit. The 750 ms cadence is a tradeoff: short enough that
/// drift is corrected within a noticeable beat, long enough that
/// re-stamping doesn't visibly compete with a child that legitimately
/// changes the title (e.g. the OSC stripper covers PTY mode, the keeper
/// is the safety net for subprocess mode).
pub fn keep_setting_in_background() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        crate::platform::console_title::spawn_keeper_thread();
    });
}

/// #706: whether the keeper thread may start, given the console probe.
///
/// Pure seam so the console-less guard is testable on every platform without
/// a real console, a thread, or `FreeConsole`.
pub fn should_spawn_keeper(console_attached: impl FnOnce() -> bool) -> bool {
    console_attached()
}

/// `GetConsoleCP` returns 0 exactly when the process has no console (#706).
pub fn code_page_means_console(code_page: u32) -> bool {
    code_page != 0
}

/// Format `<cwd-name>` into the canonical title string. Pure helper —
/// unit-tested on every platform.
pub fn title_for_cwd_name(cwd_name: &str) -> String {
    format!("clud {}", cwd_name)
}

/// Encode `title` as an OSC 2 (set window title) sequence, BEL-terminated.
///
/// Control characters are dropped so a hostile directory name cannot end
/// the sequence early and inject further escapes into the terminal.
pub fn osc2_title_bytes(title: &str) -> Vec<u8> {
    let clean: String = title.chars().filter(|c| !c.is_control()).collect();
    let mut out = Vec::with_capacity(clean.len() + 5);
    out.extend_from_slice(b"\x1b]2;");
    out.extend_from_slice(clean.as_bytes());
    out.push(0x07);
    out
}

pub fn title_for_cpu_alert(base_title: &str, cpu_pct: f32) -> String {
    format!("{base_title} CPU {:.0}%", cpu_pct)
}

pub fn current_desired_title_locked(state: &mut TitleState, now: Instant) -> String {
    let Some(alert) = &state.cpu_alert else {
        return state.base_title.clone();
    };
    if now >= alert.expires_at {
        state.cpu_alert = None;
        return state.base_title.clone();
    }
    let flash_tick = now.saturating_duration_since(alert.observed_at).as_millis()
        / CPU_FLASH_INTERVAL.as_millis();
    if flash_tick.is_multiple_of(2) {
        alert.title.clone()
    } else {
        state.base_title.clone()
    }
}

/// What the keeper learned from one snapshot check (#547).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotCheck {
    /// The file is absent — this daemon does not publish. The caller falls back
    /// to the RPC at [`CPU_METRICS_FALLBACK_POLL_INTERVAL`].
    Absent,
    /// Present and unchanged since the last look. Nothing to do, and nothing
    /// was read: this is the idle steady state, and it costs one `stat`.
    Unchanged,
    /// Present and newer than the last look — worth reading and parsing.
    Changed,
}

/// Decide whether the snapshot is worth reading, from its mtime alone.
///
/// Pure so the "idle costs one stat, zero reads" property is asserted directly
/// rather than inferred from a benchmark. `last_seen` is the mtime observed on
/// the previous pass.
pub fn classify_snapshot(
    mtime: Option<std::time::SystemTime>,
    last_seen: &mut Option<std::time::SystemTime>,
) -> SnapshotCheck {
    let Some(mtime) = mtime else {
        // Deliberately does not clear `last_seen`: a daemon restart rewrites
        // the file with a fresh mtime, which reads as Changed either way.
        return SnapshotCheck::Absent;
    };
    if *last_seen == Some(mtime) {
        return SnapshotCheck::Unchanged;
    }
    *last_seen = Some(mtime);
    SnapshotCheck::Changed
}

pub fn update_cpu_alert_locked(state: &mut TitleState, cpu_pct: f32, now: Instant) {
    if cpu_pct > CPU_FLASH_THRESHOLD_PCT && !state.base_title.is_empty() {
        state.cpu_alert = Some(CpuAlert {
            title: title_for_cpu_alert(&state.base_title, cpu_pct),
            observed_at: now,
            expires_at: now + CPU_ALERT_TTL,
        });
    } else {
        state.cpu_alert = None;
    }
}

/// Best-effort lookup of the current working directory's leaf name.
/// Returns `None` if `current_dir()` fails or the path has no final
/// component (e.g. the filesystem root on Windows like `C:\`).
fn current_cwd_basename() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .or_else(|| {
            // On `C:\` (drive root) there's no `file_name`; fall back to
            // the drive letter so the title is still informative.
            cwd.to_string_lossy()
                .split(':')
                .next()
                .map(|s| s.to_string())
        })
}

// ─── OSC 0/2 stream filter ──────────────────────────────────────────────

/// Stream-resumable filter that drops OSC 0 and OSC 2 (window-title)
/// escape sequences from a byte stream and passes everything else
/// through verbatim.
///
/// OSC syntax: `ESC ] Ps ; Pt ST` where `ST` is `BEL` (0x07) or
/// `ESC \\` (0x1B 0x5C). OSC 0 sets icon name + window title; OSC 2
/// sets only the window title. Other OSC numbers (8 hyperlinks, 10/11
/// colors, 52 clipboard, 133 prompt marks, …) pass through.
///
/// The filter survives across `process()` calls: an OSC sequence split
/// across reads is handled correctly, including ST split between two
/// chunks.
pub use console_title_osc::OscTitleStripper;

#[cfg(test)]
mod tests {
    use super::*;

    // Issue #547: the keeper backoff state machine. Pure, so the whole policy
    // is exercised without a console, a thread, or seconds of real waiting.

    #[test]
    fn a_fresh_keeper_starts_at_the_fast_cadence() {
        assert_eq!(KeeperCadence::new().interval(), KEEPER_FAST_INTERVAL);
    }

    #[test]
    fn the_cadence_holds_until_enough_quiet_passes() {
        // Backing off on the first quiet pass would slow a terminal that is
        // merely between keystrokes.
        let mut cadence = KeeperCadence::new();
        for _ in 0..(KEEPER_STABLE_PASSES_BEFORE_BACKOFF - 1) {
            cadence = cadence.record_pass(false);
            assert_eq!(cadence.interval(), KEEPER_FAST_INTERVAL);
        }
        cadence = cadence.record_pass(false);
        assert_eq!(cadence.interval(), KEEPER_FAST_INTERVAL * 2);
    }

    #[test]
    fn sustained_quiet_reaches_the_ceiling_and_stops() {
        let mut cadence = KeeperCadence::new();
        for _ in 0..200 {
            cadence = cadence.record_pass(false);
        }
        assert_eq!(
            cadence.interval(),
            KEEPER_IDLE_INTERVAL,
            "backoff must clamp, not grow without bound"
        );
    }

    #[test]
    fn any_change_snaps_straight_back_to_fast() {
        // The acceptance criterion: an externally-changed title is re-stamped
        // within one fast beat of the next check, not after a gradual ramp.
        // Responsiveness is the feature; the backoff only makes idling cheap.
        let mut cadence = KeeperCadence::new();
        for _ in 0..200 {
            cadence = cadence.record_pass(false);
        }
        assert_eq!(cadence.interval(), KEEPER_IDLE_INTERVAL);

        let after_change = cadence.record_pass(true);
        assert_eq!(
            after_change.interval(),
            KEEPER_FAST_INTERVAL,
            "a change must reset to fast in one step"
        );
    }

    /// #706: the regression the console gate exists to prevent.
    ///
    /// A console-less process (the daemon, every worker) can neither stamp a
    /// title nor read one back, so `read_console_title()` returns `None` on
    /// every pass and the keeper's `changed` flag is pinned true. This asserts
    /// the *consequence* — that such a keeper never backs off — which is why
    /// `spawn_keeper_thread` must not start one at all rather than relying on
    /// the cadence to make it cheap.
    #[test]
    fn a_keeper_that_always_reports_change_never_backs_off() {
        let mut cadence = KeeperCadence::new();
        for _ in 0..200 {
            // What the console-less loop did on every single pass.
            cadence = cadence.record_pass(true);
            assert_eq!(
                cadence.interval(),
                KEEPER_FAST_INTERVAL,
                "a pinned-change keeper stays at the fast cadence forever — \
                 the backoff cannot rescue it, so it must not be spawned"
            );
        }
    }

    // #706 / #1364: the console-less keeper guard itself.

    #[test]
    fn keeper_guard_refuses_to_spawn_without_a_console() {
        let probed = std::cell::Cell::new(false);
        assert!(!should_spawn_keeper(|| {
            probed.set(true);
            false
        }));
        assert!(probed.get(), "the guard must consult the console probe");
    }

    #[test]
    fn keeper_guard_spawns_with_a_console() {
        assert!(should_spawn_keeper(|| true));
    }

    #[test]
    fn code_page_zero_means_no_console() {
        assert!(!code_page_means_console(0));
        assert!(code_page_means_console(65001));
        assert!(code_page_means_console(437));
    }

    #[test]
    fn the_stable_counter_resets_on_change_too() {
        // Otherwise a terminal alternating change/quiet/change/quiet would
        // accumulate quiet passes across the changes and eventually back off
        // while still active.
        let mut cadence = KeeperCadence::new();
        for _ in 0..(KEEPER_STABLE_PASSES_BEFORE_BACKOFF - 1) {
            cadence = cadence.record_pass(false);
        }
        cadence = cadence.record_pass(true);
        for _ in 0..(KEEPER_STABLE_PASSES_BEFORE_BACKOFF - 1) {
            cadence = cadence.record_pass(false);
            assert_eq!(
                cadence.interval(),
                KEEPER_FAST_INTERVAL,
                "quiet passes must not carry over a change"
            );
        }
    }

    #[test]
    fn worst_case_drift_latency_stays_within_the_stated_budget() {
        // #547 budgets an above-threshold transition reaching the title within
        // 5 s. The keeper's contribution is bounded by the ceiling.
        assert!(
            KEEPER_IDLE_INTERVAL <= Duration::from_secs(5),
            "ceiling must fit the stated alert-latency budget"
        );
    }

    #[test]
    fn title_uses_clud_prefix_with_cwd_name() {
        assert_eq!(title_for_cwd_name("clud"), "clud clud");
        assert_eq!(title_for_cwd_name("my-app"), "clud my-app");
    }

    #[test]
    fn title_handles_non_ascii_cwd_name() {
        // Cyrillic + emoji to verify the formatter doesn't choke on
        // non-ASCII paths (which then flow through SetConsoleTitleW's
        // wide-string conversion on Windows).
        assert_eq!(title_for_cwd_name("проект"), "clud проект");
        assert_eq!(title_for_cwd_name("🚀"), "clud 🚀");
    }

    #[test]
    fn title_passes_through_empty_name() {
        // Defensive: even an empty cwd name produces a well-formed
        // title rather than panicking.
        assert_eq!(title_for_cwd_name(""), "clud ");
    }

    #[test]
    fn title_does_not_trim_or_normalize_input() {
        // We're explicit about what we format — no surprise trims, no
        // case changes. The cwd basename is shown verbatim.
        assert_eq!(title_for_cwd_name(" spaced "), "clud  spaced ");
        assert_eq!(title_for_cwd_name("MIXED-Case"), "clud MIXED-Case");
    }

    #[test]
    fn current_cwd_basename_returns_some_in_test_env() {
        // Cargo runs tests with the manifest dir as cwd, which always
        // has a leaf component (`clud-bin`). Smoke test the helper
        // without asserting the specific value.
        let got = current_cwd_basename();
        assert!(got.is_some(), "expected Some(_) cwd basename in test env");
        assert!(!got.unwrap().is_empty(), "basename should not be empty");
    }

    #[test]
    fn osc2_title_bytes_wraps_title_in_osc_2() {
        assert_eq!(
            osc2_title_bytes("clud clud2"),
            b"\x1b]2;clud clud2\x07".to_vec()
        );
    }

    #[test]
    fn osc2_title_bytes_strips_control_chars() {
        assert_eq!(
            osc2_title_bytes("clud a\x07\x1b]0;evil\nb"),
            b"\x1b]2;clud a]0;evilb\x07".to_vec()
        );
    }

    #[test]
    fn osc2_title_bytes_round_trips_through_the_stripper() {
        // Our own stamp must be recognised (and swallowed) by the PTY
        // stripper like any child's title, so the two stay consistent.
        let mut s = OscTitleStripper::new();
        assert!(s.process(&osc2_title_bytes("clud x")).is_empty());
    }

    #[test]
    fn set_for_current_cwd_does_not_panic() {
        // Smoke test on every platform — on POSIX stdout is not a tty under the test harness,
        // and on Windows SetConsoleTitleW returns silently when there
        // is no console (e.g. inside `cargo test` under a CI runner).
        set_for_current_cwd();
    }

    #[test]
    fn set_for_current_cwd_records_desired_title() {
        // The keeper thread reads from this cell to decide whether to
        // re-stamp; if the cell stays empty after `set_for_current_cwd`,
        // the keeper would idle forever. Verify the value is captured.
        set_for_current_cwd();
        let stored = title_state_cell()
            .lock()
            .expect("title state mutex")
            .base_title
            .clone();
        assert!(
            stored.starts_with("clud "),
            "desired title should be the formatted form, got {stored:?}"
        );
        assert!(
            !stored.trim_end().eq("clud"),
            "desired title should include a basename component, got {stored:?}"
        );
    }

    #[test]
    fn cpu_alert_title_formats_percentage() {
        assert_eq!(title_for_cpu_alert("clud repo", 72.4), "clud repo CPU 72%");
        assert_eq!(title_for_cpu_alert("clud repo", 72.5), "clud repo CPU 72%");
        assert_eq!(title_for_cpu_alert("clud repo", 72.6), "clud repo CPU 73%");
    }

    #[test]
    fn cpu_alert_flashes_between_alert_and_base_title() {
        let now = Instant::now();
        let mut state = TitleState {
            base_title: "clud repo".to_string(),
            cpu_alert: None,
        };
        update_cpu_alert_locked(&mut state, 71.0, now);

        assert_eq!(
            current_desired_title_locked(&mut state, now),
            "clud repo CPU 71%"
        );
        assert_eq!(
            current_desired_title_locked(&mut state, now + CPU_FLASH_INTERVAL),
            "clud repo"
        );
        assert_eq!(
            current_desired_title_locked(&mut state, now + CPU_FLASH_INTERVAL * 2),
            "clud repo CPU 71%"
        );
    }

    #[test]
    fn cpu_alert_clears_below_threshold_and_after_ttl() {
        let now = Instant::now();
        let mut state = TitleState {
            base_title: "clud repo".to_string(),
            cpu_alert: None,
        };

        update_cpu_alert_locked(&mut state, 70.0, now);
        assert!(state.cpu_alert.is_none(), "70% is not above threshold");

        update_cpu_alert_locked(&mut state, 70.1, now);
        assert!(state.cpu_alert.is_some());

        update_cpu_alert_locked(&mut state, 10.0, now + Duration::from_secs(1));
        assert!(state.cpu_alert.is_none());

        update_cpu_alert_locked(&mut state, 80.0, now);
        assert_eq!(
            current_desired_title_locked(&mut state, now + CPU_ALERT_TTL),
            "clud repo"
        );
        assert!(state.cpu_alert.is_none());
    }

    #[test]
    fn keep_setting_in_background_is_idempotent_and_does_not_panic() {
        // Calling more than once must not spawn duplicate keeper
        // threads (OnceLock guard) and must not panic on POSIX where
        // the spawn helper is a no-op.
        keep_setting_in_background();
        keep_setting_in_background();
    }

    // ─── OscTitleStripper ──────────────────────────────────────────────
    #[test]
    fn parameterless_osc_passes_through_with_either_terminator() {
        let mut s = OscTitleStripper::new();
        assert_eq!(s.process(b"\x1b]104\x07"), b"\x1b]104\x07");
        let mut s = OscTitleStripper::new();
        assert_eq!(s.process(b"\x1b]112\x1b\\after"), b"\x1b]112\x1b\\after");
    }

    #[test]
    fn osc_stripper_passthrough_for_plain_bytes() {
        let mut s = OscTitleStripper::new();
        assert_eq!(s.process(b"hello world\n"), b"hello world\n");
    }

    #[test]
    fn osc_stripper_drops_osc_0_with_bel_terminator() {
        let mut s = OscTitleStripper::new();
        let chunk = b"before\x1b]0;child-title\x07after";
        assert_eq!(s.process(chunk), b"beforeafter");
    }

    #[test]
    fn osc_stripper_drops_osc_2_with_st_terminator() {
        let mut s = OscTitleStripper::new();
        // ST = ESC \ (0x1B 0x5C)
        let chunk = b"x\x1b]2;another-title\x1b\\y";
        assert_eq!(s.process(chunk), b"xy");
    }

    #[test]
    fn osc_stripper_passes_through_osc_10_color_query() {
        // OSC 10 is the foreground-color query; vt100-class TUIs send
        // it to discover the terminal palette. Stripping it would hang
        // the child waiting for a reply that never comes.
        let mut s = OscTitleStripper::new();
        let chunk = b"\x1b]10;?\x07";
        assert_eq!(s.process(chunk), b"\x1b]10;?\x07");
    }

    #[test]
    fn osc_stripper_passes_through_osc_8_hyperlink() {
        let mut s = OscTitleStripper::new();
        // OSC 8 ; ; <url> ST <text> OSC 8 ; ; ST
        let chunk = b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\";
        assert_eq!(s.process(chunk), chunk);
    }

    #[test]
    fn osc_stripper_passes_through_osc_133_prompt_marks() {
        let mut s = OscTitleStripper::new();
        let chunk = b"\x1b]133;A\x07$ ls\n\x1b]133;B\x07";
        assert_eq!(s.process(chunk), chunk);
    }

    #[test]
    fn osc_stripper_handles_split_across_chunks() {
        // Worst-case: OSC sequence split into many tiny chunks. The
        // filter must reassemble the prefix decision and the ST
        // detection across calls.
        let mut s = OscTitleStripper::new();
        let mut got = Vec::new();
        for piece in [
            &b"abc\x1b"[..],
            b"]",
            b"0",
            b";",
            b"split-title",
            b"\x07",
            b"xyz",
        ] {
            got.extend(s.process(piece));
        }
        assert_eq!(got, b"abcxyz");
    }

    #[test]
    fn osc_stripper_handles_st_split_across_chunks() {
        let mut s = OscTitleStripper::new();
        let mut got = Vec::new();
        got.extend(s.process(b"\x1b]0;t\x1b"));
        got.extend(s.process(b"\\tail"));
        assert_eq!(got, b"tail");
    }

    #[test]
    fn osc_stripper_handles_back_to_back_title_oscs() {
        let mut s = OscTitleStripper::new();
        let chunk = b"a\x1b]0;t1\x07b\x1b]2;t2\x1b\\c";
        assert_eq!(s.process(chunk), b"abc");
    }

    #[test]
    fn osc_stripper_lone_esc_is_buffered_until_resolved() {
        // A bare ESC by itself isn't emitted until we know whether it's
        // starting an OSC — but if the next byte isn't `]`, both bytes
        // must surface. This protects CSI sequences (ESC [ …) from
        // being eaten.
        let mut s = OscTitleStripper::new();
        assert_eq!(s.process(b"\x1b[31mred\x1b[0m"), b"\x1b[31mred\x1b[0m");
    }

    #[test]
    fn osc_stripper_double_esc_does_not_eat_either() {
        // ESC ESC ] 0 ; … BEL: the first ESC isn't OSC-related, the
        // second one starts an OSC. We must emit the first ESC.
        let mut s = OscTitleStripper::new();
        assert_eq!(s.process(b"\x1b\x1b]0;x\x07"), b"\x1b");
    }

    #[test]
    fn osc_stripper_malformed_osc_with_letter_passes_through() {
        // OSC `]X;` is bogus — but conservatively pass it through
        // rather than swallowing arbitrary bytes that might be
        // user-visible. Real terminals would also pass it through.
        let mut s = OscTitleStripper::new();
        let chunk = b"\x1b]X;weird\x07";
        assert_eq!(s.process(chunk), chunk);
    }

    #[test]
    fn osc_stripper_multidigit_non_title_passthrough() {
        // OSC 52 (clipboard) starts with `5` — must not be confused
        // with `2`. The digit accumulator handles multi-digit numbers.
        let mut s = OscTitleStripper::new();
        let chunk = b"\x1b]52;c;SGVsbG8=\x07";
        assert_eq!(s.process(chunk), chunk);
    }

    // ---- #547: publish/stat replaces the per-client 2 s poll ----

    use std::time::{Duration as StdDuration, SystemTime};

    /// The acceptance criterion, stated as a count: once the daemon publishes,
    /// an idle client performs **zero** reads (and therefore zero daemon
    /// connections) while nothing changes. Before #547 this path was one
    /// `Metrics` round-trip every 2 s, forever.
    #[test]
    fn an_idle_client_reads_the_snapshot_zero_times() {
        let mtime = SystemTime::UNIX_EPOCH + StdDuration::from_secs(1_000);
        let mut last_seen = None;

        // First look: the file is new to us, so it is read once.
        assert_eq!(
            classify_snapshot(Some(mtime), &mut last_seen),
            SnapshotCheck::Changed
        );

        // Every subsequent pass over an unchanged file costs one `stat` and
        // nothing else. 200 passes ~ 10 minutes of idle at the fast cadence.
        let reads = (0..200)
            .filter(|_| classify_snapshot(Some(mtime), &mut last_seen) == SnapshotCheck::Changed)
            .count();
        assert_eq!(reads, 0, "an idle client must not read the snapshot at all");
    }

    /// The falling edge has to reach the client too, or an alert sticks. A
    /// rewritten file has a new mtime, so it is read again.
    #[test]
    fn a_republished_snapshot_is_read_again() {
        let mut last_seen = None;
        let first = SystemTime::UNIX_EPOCH + StdDuration::from_secs(1_000);
        let second = first + StdDuration::from_secs(2);

        assert_eq!(
            classify_snapshot(Some(first), &mut last_seen),
            SnapshotCheck::Changed
        );
        assert_eq!(
            classify_snapshot(Some(first), &mut last_seen),
            SnapshotCheck::Unchanged
        );
        assert_eq!(
            classify_snapshot(Some(second), &mut last_seen),
            SnapshotCheck::Changed
        );
    }

    /// An older daemon publishes nothing. The client must notice and fall back
    /// to the RPC rather than silently losing the alert -- graceful degradation
    /// is what makes the file the *preferred* path rather than the only one.
    #[test]
    fn an_absent_snapshot_reports_absent_so_the_caller_can_fall_back() {
        let mut last_seen = None;
        assert_eq!(
            classify_snapshot(None, &mut last_seen),
            SnapshotCheck::Absent
        );
        assert!(
            last_seen.is_none(),
            "absence must not be recorded as a seen mtime"
        );
    }

    /// A daemon restart republishes with a fresh mtime, and the client must
    /// pick that up even though it had already seen an earlier file.
    #[test]
    fn a_daemon_restart_is_observed_through_the_absent_gap() {
        let mut last_seen = None;
        let before = SystemTime::UNIX_EPOCH + StdDuration::from_secs(1_000);
        let after = before + StdDuration::from_secs(30);

        assert_eq!(
            classify_snapshot(Some(before), &mut last_seen),
            SnapshotCheck::Changed
        );
        // Daemon down: file removed.
        assert_eq!(
            classify_snapshot(None, &mut last_seen),
            SnapshotCheck::Absent
        );
        // Daemon back, having republished.
        assert_eq!(
            classify_snapshot(Some(after), &mut last_seen),
            SnapshotCheck::Changed
        );
    }

    /// Alert latency budget from #547: an above-threshold transition must reach
    /// the title within 5 s. The daemon samples every 2 s and the client checks
    /// every keeper pass, whose *slowest* cadence is the idle ceiling.
    #[test]
    fn the_alert_latency_budget_is_met_by_construction() {
        let worst_case = crate::daemon::CPU_SAMPLE_INTERVAL_FOR_TEST + KEEPER_IDLE_INTERVAL;
        assert!(
            worst_case <= Duration::from_secs(5),
            "worst-case alert latency {worst_case:?} exceeds the 5s budget"
        );
    }
}

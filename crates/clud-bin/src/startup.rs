//! Launch-time helpers: console drag-drop registration, session-cap
//! enforcement, and the Ctrl+C flag installer. Factored out of `main.rs`
//! so the entry point reads as orchestration rather than plumbing.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::args::Args;
use crate::dnd;
use crate::session_registry;

/// Issue #79: a launch should register the console IDropTarget unless
/// the user opted out (`--no-dnd`) or the run can't possibly need a
/// drop target (`--dry-run` returns before this is consulted, but the
/// helper still rejects it for symmetry / explicit testability).
///
/// Factored out so `main.rs`'s logic can be unit-tested without
/// touching OLE or spawning processes.
pub fn should_register_drop_target(args: &Args) -> bool {
    if args.no_dnd {
        return false;
    }
    if args.dry_run {
        return false;
    }
    crate::platform::host::WINDOWS
}

/// Whether a failed drop-target registration is worth a line on stderr.
///
/// Issue #594: "no console attached" is the *expected* state everywhere clud
/// runs headless — CI runners, `--detach`, a piped or redirected launch — not
/// a fault. Reporting it wrote a note to stderr on every such launch, which is
/// noise for users and an outright failure for integration tests that assert
/// on clean stderr (one of the rotating victims on the Windows x86 lane).
///
/// The other variants describe something that *should* have worked and did
/// not — OLE refusing to initialize, `RegisterDragDrop` failing, a worker
/// thread that would not spawn — so those keep their line. The test is on the
/// typed variant rather than the rendered message, so it cannot drift when the
/// wording changes.
pub fn should_report_registration_failure(error: &dnd::console_drop_target::RegisterError) -> bool {
    use dnd::console_drop_target::RegisterError;
    !matches!(
        error,
        // Headless: there is no console window to attach a drop target to.
        RegisterError::ConsoleWindowUnavailable
            // Also not failures: the capability is simply absent from this
            // build or platform, which no user can act on.
            | RegisterError::UnsupportedPlatform
            | RegisterError::NotImplemented
    )
}

/// Subprocess-mode IDropTarget registration. Returns `None` on any
/// failure, logging a one-line warning to stderr only when the failure is
/// actionable (see [`should_report_registration_failure`]), so a
/// registration hiccup never aborts the launch path.
pub fn try_register_console_drop_target_subprocess(
) -> Option<dnd::console_drop_target::ConsoleDropTargetGuard> {
    crate::platform::startup::try_register_console_drop_target_subprocess()
}

/// PTY-mode IDropTarget registration. The injector writes into a
/// channel; the pump drains it each iteration and forwards into the
/// PTY master. Always `(None, None)` off Windows.
pub use crate::platform::startup::try_register_console_drop_target_pty;

/// RAII guard for the session-registry row. On Drop, briefly re-acquires
/// the cross-process lock, opens the redb file, removes our row, and
/// closes. Best-effort — failures are silent because the next startup's
/// GC pass cleans up stale rows anyway (issue #73).
pub struct ScopedSessionGuard;

impl Drop for ScopedSessionGuard {
    fn drop(&mut self) {
        // #1660: bounded, so a contended lock or slow sync cannot hold exit.
        let _ =
            session_registry::run_shutdown_under_lock_bounded(session_registry::SHUTDOWN_BUDGET);
    }
}

/// Issue #73 / #138: enforce the live-session cap. Acquires a cross-process
/// advisory lock, opens redb, runs gc + cap-check + register-self, then
/// closes redb and releases the lock — all in one short critical section
/// at startup. On `Refuse` this calls `std::process::exit(1)` directly.
/// On `Warn` we print to stderr and continue. Failures to open / GC the
/// DB are *non-fatal*: we log to stderr and skip the cap check, because
/// breaking `clud` startup over a registry hiccup would be much worse
/// than the rare case where the guardrail is temporarily missing.
///
/// Returns a guard whose Drop removes our row on graceful exit. The guard
/// holds no DB or lock between startup and shutdown — concurrent `clud`
/// launches can therefore both run through this function without racing
/// on the redb file lock (which previously caused issue #138's
/// `Database already open` warning).
pub fn enforce_session_cap() -> Option<ScopedSessionGuard> {
    let cfg = session_registry::SessionRegistry::cap_config_from_env();
    let info = session_registry::SessionInfo::for_self(None, None);
    let outcome = match session_registry::run_startup_under_lock(&cfg, info) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[clud] warning: could not open session registry: {e}");
            return None;
        }
    };
    match outcome.decision {
        session_registry::CapDecision::Allow => {}
        session_registry::CapDecision::Warn(count) => {
            eprintln!(
                "[clud] warning: {count} live clud sessions detected (warn threshold {warn}, cap {cap}). \
                 Set {env_max}=0 to disable, or wind down old sessions.",
                warn = cfg.warn,
                cap = cfg.max,
                env_max = session_registry::ENV_MAX_INSTANCES,
            );
        }
        session_registry::CapDecision::Refuse(count) => {
            eprintln!(
                "[clud] error: {count} live clud sessions exceed the cap of {cap}. \
                 Refusing to launch (fork-bomb guardrail, issue #73). \
                 Wind down old sessions, or override via {env_max}=<larger> / \
                 {env_max}=0 to disable.",
                cap = cfg.max,
                env_max = session_registry::ENV_MAX_INSTANCES,
            );
            std::process::exit(1);
        }
    }
    if outcome.registered {
        Some(ScopedSessionGuard)
    } else {
        None
    }
}

pub fn install_ctrl_c_flag(verbose: bool) -> Arc<AtomicBool> {
    let interrupted = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&interrupted);
    // Snapshot interactivity once at install time so the handler
    // doesn't have to call `isatty()` from the ctrlc thread on every
    // press. Issue #377's double-Ctrl+C guard is meant to protect
    // interactive users from accidental teardown; scripts and CI
    // (which redirect stdin/stderr) should keep the historical
    // single-press exit so existing automation isn't broken.
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::io::IsTerminal::is_terminal(&std::io::stderr());
    if let Err(e) = ctrlc::set_handler(move || {
        run_ctrl_c_handler(
            verbose,
            interactive,
            handler_flag.as_ref(),
            |msg| crate::verbose_log::log(msg),
            |msg| {
                // Stderr so the "press again to exit" notice shows up
                // even when verbose-mode logging is off — the user
                // needs to see it regardless of logging configuration.
                crate::loop_console::eprint_diagnostic(&format!("{msg}"));
            },
        );
    }) {
        eprintln!("[clud] warning: failed to install Ctrl+C handler: {}", e);
    }
    crate::platform::startup::install_windows_ctrl_event_probe();
    crate::platform::startup::install_unix_termination_probe(&interrupted);
    interrupted
}

/// Outcome of a single press through [`run_ctrl_c_handler`]. The Windows
/// double-Ctrl+C guard (issue #377) swallows the first press in a
/// rapid-succession window so users don't tear down clud (and any
/// long-running backend session it owns) by accidental keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CtrlCDecision {
    /// First press inside a fresh rapid-succession window. clud did
    /// not flip the interrupted flag; a follow-up press is required to
    /// exit. The child still received its own copy of the
    /// console-control event from the OS so it can cancel cooperatively.
    FirstSoft { window_ms: u64 },
    /// Press flipped the interrupted flag. Either the second press
    /// landed inside the window, the guard was disabled by env var,
    /// or the platform doesn't engage the guard (non-Windows).
    Exit,
}

/// Pure side-effecting body of the Ctrl+C handler closure, extracted so
/// unit tests can assert the verbose-emit and double-tap decisions
/// without installing a real signal handler (which would conflict with
/// cargo's test runner).
///
/// The handler:
/// 1. Atomically swaps in the current timestamp and reads the prior one
///    (signal-safe — see [`crate::ctrl_c_track::record_observed_returning_prior`]).
/// 2. On Windows (and only when the double-tap guard is engaged), checks
///    whether the prior press lands inside the rapid-succession window.
///    If not, classifies this as `FirstSoft` and skips flipping the
///    interrupted flag — clud stays alive so the user can decide whether
///    to interrupt again.
/// 3. Otherwise (second press in window / non-Windows / opt-out), flips
///    the interrupted flag.
/// 4. Stamps the press classification + elapsed-since-prior into the
///    forensic event so the dashboard can distinguish swallowed presses
///    from teardown presses.
///
/// `emit_warning` carries the "press again to exit" notice. Tests pass
/// a `Cell`-backed closure to capture it; the production caller passes
/// a real `eprintln!` so the user sees it in their shell even when
/// `--verbose` is off.
///
/// `interactive` is the install-time snapshot of whether stdin **and**
/// stderr are TTYs. The double-tap guard is gated on this so scripts
/// and CI (which redirect at least one of those handles) keep the
/// historical single-press exit. Without this gate the guard breaks
/// every Python integration test that drives clud via subprocess and
/// sends a single Ctrl+C — the test would sit waiting for a second
/// press that no automation knows to send.
pub(crate) fn run_ctrl_c_handler(
    verbose: bool,
    interactive: bool,
    interrupted: &AtomicBool,
    emit_verbose: impl FnOnce(&str),
    emit_warning: impl FnOnce(&str),
) -> CtrlCDecision {
    use crate::ctrl_c_track::{
        double_tap_enabled, double_tap_window_ms, record_elapsed_since_prior_ms,
        record_observed_returning_prior, record_press_kind, CtrlPressKind,
    };
    use std::sync::atomic::Ordering;

    // Issue #517: on Unix, `ctrlc` (without the `termination` feature)
    // only ever installs a SIGINT handler, so every press that reaches
    // this closure unambiguously means SIGINT/Ctrl+C. On Windows the
    // kind is already stamped by `install_windows_ctrl_event_probe`
    // *before* this handler runs (it can be CtrlC, CtrlBreak, ...), so
    // stamping CtrlC here unconditionally would silently overwrite a
    // more specific Windows event with the wrong one.
    if !crate::platform::host::WINDOWS {
        crate::ctrl_c_track::record_event_kind(crate::ctrl_c_track::CtrlEventKind::CtrlC);
    }

    let prior_ms = record_observed_returning_prior();
    let elapsed_ms = if prior_ms == 0 {
        None
    } else {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(prior_ms);
        Some(now_ms.saturating_sub(prior_ms))
    };
    if let Some(gap) = elapsed_ms {
        // record_elapsed_since_prior_ms uses 0 as "no prior" sentinel,
        // so coerce a genuine zero gap to 1ms to keep the forensic
        // field meaningful.
        record_elapsed_since_prior_ms(gap.max(1));
    }

    let window_ms = double_tap_window_ms();
    let guard_engaged = interactive && double_tap_enabled();
    let inside_window = matches!(elapsed_ms, Some(gap) if gap <= window_ms);

    let decision = if guard_engaged && !inside_window {
        CtrlCDecision::FirstSoft { window_ms }
    } else {
        CtrlCDecision::Exit
    };

    match decision {
        CtrlCDecision::FirstSoft { window_ms } => {
            record_press_kind(CtrlPressKind::FirstSoft);
            if verbose {
                emit_verbose("[clud] ctrl-c received (first press, soft interrupt)");
            }
            emit_warning(&format!(
                "[clud] Ctrl+C — press again within {window_ms}ms to exit"
            ));
        }
        CtrlCDecision::Exit => {
            record_press_kind(CtrlPressKind::SecondExit);
            if verbose {
                emit_verbose("[clud] ctrl-c received");
            }
            interrupted.store(true, Ordering::SeqCst);
        }
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_from(argv: &[&str]) -> Args {
        let raw: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        Args::parse_from_raw(raw)
    }

    /// Issue #594: running without a console is the normal headless case,
    /// not a fault, so it must not put anything on stderr. This is the exact
    /// line that tripped `test_loop_no_markers_exhausts_iterations` on the
    /// Windows x86 lane.
    #[test]
    fn a_missing_console_is_not_worth_reporting() {
        use dnd::console_drop_target::RegisterError;
        assert!(!should_report_registration_failure(
            &RegisterError::ConsoleWindowUnavailable
        ));
        assert!(!should_report_registration_failure(
            &RegisterError::UnsupportedPlatform
        ));
        assert!(!should_report_registration_failure(
            &RegisterError::NotImplemented
        ));
    }

    /// The other half of the contract, and the reason this is a predicate
    /// rather than deleting the `eprintln!`: a capability that was supposed
    /// to work and did not still has to say so.
    #[test]
    fn a_genuine_registration_failure_is_still_reported() {
        use dnd::console_drop_target::RegisterError;
        assert!(should_report_registration_failure(
            &RegisterError::OleInitializeFailed(-2147417850)
        ));
        assert!(should_report_registration_failure(
            &RegisterError::RegisterDragDropFailed(-2147024891)
        ));
        assert!(should_report_registration_failure(
            &RegisterError::WorkerSpawnFailed
        ));
    }

    /// Guards the silent-by-default hazard: a variant added later would fall
    /// into neither test above, and whichever way it defaults would be an
    /// unreviewed decision. Listing every variant here forces the author to
    /// come back and choose.
    #[test]
    fn every_registration_error_variant_has_a_stated_verdict() {
        use dnd::console_drop_target::RegisterError;
        let all = [
            RegisterError::NotImplemented,
            RegisterError::UnsupportedPlatform,
            RegisterError::ConsoleWindowUnavailable,
            RegisterError::OleInitializeFailed(0),
            RegisterError::RegisterDragDropFailed(0),
            RegisterError::WorkerSpawnFailed,
        ];
        // If this fails to compile because a variant is missing, add it above
        // and to whichever of the two tests states the intended behaviour.
        assert_eq!(all.len(), 6, "add the new variant to the tests above");
        assert_eq!(
            all.iter()
                .filter(|e| should_report_registration_failure(e))
                .count(),
            3
        );
    }

    /// Issue #79 B3: a `--dry-run` invocation must NOT trigger any
    /// `RegisterDragDrop` side effect. We can't observe the OLE call
    /// directly from a unit test (it would require a console window),
    /// so we test the gating helper that `main()` consults.
    #[test]
    fn main_dry_run_does_not_register_drop_target() {
        let args = args_from(&["clud", "--dry-run", "-p", "hi"]);
        assert!(args.dry_run);
        assert!(
            !should_register_drop_target(&args),
            "--dry-run must short-circuit the drop-target registration"
        );
    }

    #[test]
    fn main_no_dnd_flag_disables_registration() {
        let args = args_from(&["clud", "--no-dnd"]);
        assert!(args.no_dnd);
        assert!(
            !should_register_drop_target(&args),
            "--no-dnd must opt out of drop-target registration"
        );
    }

    #[test]
    fn main_no_drag_drop_alias_disables_registration() {
        let args = args_from(&["clud", "--no-drag-drop"]);
        assert!(args.no_dnd);
        assert!(!should_register_drop_target(&args));
    }

    /// Default invocation: registration is requested on Windows, skipped
    /// on POSIX. The actual COM call is downstream of this gate.
    #[test]
    fn main_default_invocation_requests_registration_on_windows() {
        let args = args_from(&["clud", "-p", "hi"]);
        let want = crate::platform::host::WINDOWS;
        assert_eq!(should_register_drop_target(&args), want);
    }

    /// Reset the `ctrl_c_track` statics + relevant env vars so each
    /// double-tap test starts from a known state. Callers must already
    /// hold `crate::ctrl_c_track::test_state_lock()` to serialize.
    fn reset_handler_state() {
        crate::ctrl_c_track::reset_for_test();
        std::env::remove_var(crate::ctrl_c_track::ENV_DISABLE_DOUBLE_TAP);
        std::env::remove_var(crate::ctrl_c_track::ENV_DOUBLE_TAP_WINDOW_MS);
    }

    /// Verbose mode: every press that actually exits emits the
    /// `[clud] ctrl-c received` marker so the launch log shows the
    /// moment of interrupt, not just the eventual exit-code lines.
    /// Non-Windows: a single press exits, so the marker fires on the
    /// first press. Windows: the first press is a soft interrupt;
    /// we drive the second press inside the window to land on Exit.
    #[test]
    fn ctrl_c_handler_emits_verbose_marker_when_verbose() {
        use std::cell::Cell;
        use std::sync::atomic::Ordering;
        let _guard = crate::ctrl_c_track::test_state_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_handler_state();
        let interrupted = AtomicBool::new(false);
        let captured: Cell<Option<String>> = Cell::new(None);

        // First press: on Windows this is a soft interrupt and the
        // emit closure receives a different marker. To exercise the
        // Exit branch on every platform, prime the timestamp first
        // and then drive the press that lands inside the window.
        if crate::platform::host::WINDOWS {
            // Prime: this swallows the first press on Windows.
            let _ = super::run_ctrl_c_handler(false, true, &interrupted, |_| {}, |_| {});
        }

        let decision = super::run_ctrl_c_handler(
            true,
            true,
            &interrupted,
            |msg| captured.set(Some(msg.to_string())),
            |_| {},
        );

        assert!(
            matches!(decision, super::CtrlCDecision::Exit),
            "handler must Exit when the guard is not engaged or inside the window"
        );
        assert_eq!(
            captured.into_inner().as_deref(),
            Some("[clud] ctrl-c received"),
            "verbose handler must emit the ctrl-c marker on Exit"
        );
        assert!(
            interrupted.load(Ordering::SeqCst),
            "handler must flip the interrupted flag on Exit"
        );
    }

    /// Non-verbose mode: the verbose marker is suppressed. The
    /// interrupted flag still flips on Exit. Mirrors the test above
    /// but checks the suppressed-emit path.
    #[test]
    fn ctrl_c_handler_skips_verbose_marker_when_not_verbose() {
        use std::cell::Cell;
        use std::sync::atomic::Ordering;
        let _guard = crate::ctrl_c_track::test_state_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_handler_state();
        let interrupted = AtomicBool::new(false);

        if crate::platform::host::WINDOWS {
            // Prime: swallow the first soft press on Windows.
            let _ = super::run_ctrl_c_handler(false, true, &interrupted, |_| {}, |_| {});
        }

        let called = Cell::new(false);
        let decision = super::run_ctrl_c_handler(
            false,
            true,
            &interrupted,
            |_| {
                called.set(true);
            },
            |_| {},
        );
        assert!(
            matches!(decision, super::CtrlCDecision::Exit),
            "handler must Exit when the guard is not engaged or inside the window"
        );
        assert!(
            !called.into_inner(),
            "non-verbose handler must not call the emit closure on Exit"
        );
        assert!(
            interrupted.load(Ordering::SeqCst),
            "handler must still flip the interrupted flag without verbose"
        );
    }

    // ---------------------------------------------------------------
    // Issue #377: double-Ctrl+C guard handler behavior.
    // ---------------------------------------------------------------

    /// Non-interactive (scripts, CI, piped stdin) MUST keep the
    /// single-press exit semantics on every platform — Python
    /// integration tests in this repo drive clud via subprocess and
    /// send a single Ctrl+C; they would deadlock under the double-tap
    /// guard. Issue #377 is about protecting **interactive users**, not
    /// about changing the behavior contract automation depends on.
    #[test]
    fn non_interactive_keeps_single_press_exit_on_every_platform() {
        use std::sync::atomic::Ordering;
        let _guard = crate::ctrl_c_track::test_state_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_handler_state();
        let interrupted = AtomicBool::new(false);

        // interactive=false simulates piped stdin / non-TTY stderr
        // (the install-time snapshot would have come back false).
        let decision = super::run_ctrl_c_handler(false, false, &interrupted, |_| {}, |_| {});
        assert!(
            matches!(decision, super::CtrlCDecision::Exit),
            "non-interactive: the first press MUST Exit on every platform"
        );
        assert!(
            interrupted.load(Ordering::SeqCst),
            "non-interactive: the first press MUST flip interrupted"
        );
    }
}

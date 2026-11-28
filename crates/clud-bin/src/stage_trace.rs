//! Stage breadcrumbs for a launch that never exits (#594, #1168).
//!
//! `run_clud` in the integration harness points `CLUD_EXIT_TIMING_FILE` at a
//! scratch file on every launch and, when it has to kill a process that blew
//! its budget, renders that file: a stage that was entered and never
//! completed is where the process was stuck. The exit stages in `main.rs`
//! have used this since #594. #1168 showed the file empty on a wedged
//! `clud -p` run whose backend had already finished, so the launch phase --
//! spawn, wait, child teardown, the drops between the runner returning and
//! the exit stages starting -- now leaves the same breadcrumbs.
//!
//! Each line is `<kind>-stage begin <name>` followed by
//! `<kind>-stage done <name>=<ms>ms`; the harness only cares that a `begin`
//! has a matching `done`. Writes are best-effort and append-only so a process
//! killed mid-write still leaves every earlier line readable.
//!
//! Two sinks, deliberately. stderr stays behind `enabled` (`--verbose` or
//! `CLUD_EXIT_TIMING`), because an unconditional line there would break every
//! test asserting clean stderr. The file sink never touches stderr, so the
//! harness can enable attribution on every launch without perturbing an
//! assertion, and still recover the trace from a process it had to kill.

use std::time::Instant;

use crate::verbose_log;

/// Which phase a stage belongs to. Only affects the line prefix; the harness
/// treats both alike when pairing `begin` with `done`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Spawning and waiting on the backend, and the drops that follow the
    /// runner returning. Empty in #1168's trace, which is why it exists.
    Launch,
    /// The post-run cleanup stages `main.rs` has traced since #594.
    Exit,
}

impl Phase {
    fn prefix(self) -> &'static str {
        match self {
            Phase::Launch => "launch-stage",
            Phase::Exit => "exit-stage",
        }
    }
}

/// True when breadcrumbs should also go to stderr via `verbose_log`.
pub fn stderr_enabled(verbose: bool) -> bool {
    verbose || std::env::var_os("CLUD_EXIT_TIMING").is_some()
}

/// True when any sink is live, so callers can skip formatting on the default
/// path where neither `--verbose` nor the trace file is set.
pub fn active(enabled: bool) -> bool {
    enabled || std::env::var_os("CLUD_EXIT_TIMING_FILE").is_some()
}

/// Write one raw line to every live sink.
pub fn trace(enabled: bool, line: &str) {
    if enabled {
        verbose_log::log(format_args!("[clud] {line}"));
    }
    if let Some(path) = std::env::var_os("CLUD_EXIT_TIMING_FILE") {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }
}

/// Record that `name` started; returns the instant to hand back to [`done`].
pub fn begin(enabled: bool, phase: Phase, name: &str) -> Instant {
    if active(enabled) {
        trace(enabled, &format!("{} begin {name}", phase.prefix()));
    }
    Instant::now()
}

/// Record that `name` finished, returning its wall-clock cost in ms.
pub fn done(enabled: bool, phase: Phase, name: &str, started: Instant) -> u128 {
    let ms = started.elapsed().as_millis();
    if active(enabled) {
        trace(enabled, &format!("{} done {name}={ms}ms", phase.prefix()));
    }
    ms
}

/// Run `body` between a `begin`/`done` pair. Keeps call sites to one line so
/// adding a breadcrumb around an existing drop or call is not a refactor.
pub fn scoped<T>(enabled: bool, phase: Phase, name: &str, body: impl FnOnce() -> T) -> T {
    let started = begin(enabled, phase, name);
    let out = body();
    done(enabled, phase, name, started);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The sink is process-global env, so tests that set it must not overlap.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn launch_and_exit_stages_share_the_file_with_distinct_prefixes() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = dir.path().join("trace.log");
        std::env::set_var("CLUD_EXIT_TIMING_FILE", &path);

        scoped(false, Phase::Launch, "child_wait", || {});
        scoped(false, Phase::Exit, "scan_and_report", || {});

        std::env::remove_var("CLUD_EXIT_TIMING_FILE");
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 4, "{body}");
        assert_eq!(lines[0], "launch-stage begin child_wait");
        assert!(
            lines[1].starts_with("launch-stage done child_wait="),
            "{body}"
        );
        assert!(lines[1].ends_with("ms"), "{body}");
        assert_eq!(lines[2], "exit-stage begin scan_and_report");
        assert!(
            lines[3].starts_with("exit-stage done scan_and_report="),
            "{body}"
        );
    }

    #[test]
    fn begin_lands_before_body_runs_so_a_killed_body_is_attributable() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = dir.path().join("trace.log");
        std::env::set_var("CLUD_EXIT_TIMING_FILE", &path);

        let seen_inside = scoped(false, Phase::Launch, "backend_run", || {
            std::fs::read_to_string(&path).unwrap()
        });

        std::env::remove_var("CLUD_EXIT_TIMING_FILE");
        assert_eq!(seen_inside.trim(), "launch-stage begin backend_run");
    }

    #[test]
    fn nothing_is_written_without_a_sink() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CLUD_EXIT_TIMING_FILE");
        assert!(!active(false));
        // Must not panic or touch the filesystem; the return value is the
        // body's, untouched.
        assert_eq!(scoped(false, Phase::Exit, "noop", || 7), 7);
    }
}

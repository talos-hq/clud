//! Inter-iteration pacing for `clud loop` (#2233).
//!
//! An iteration that finishes in seconds restarts the backend immediately, and
//! every restart is a brand-new provider session paying for the whole system
//! prompt, the global `CLAUDE.md` and the task text again. Measured on one
//! fleet coordinator running `clud loop --loop-count 100000`: 978 iterations in
//! 5 h (~18 s apart) for a `systemctl is-active` poll, which was 428 MB (27 %)
//! of that day's context exposure for the whole fleet.
//!
//! `--interval` / `loop.json: interval` makes the wait between iterations
//! real, so the cheapest correct agent move (end the turn) is also the one the
//! loop actually honours. `--repeat` is different: it sleeps *after* the whole
//! loop has finished, before re-running it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

/// Smallest accepted inter-iteration wait: one second.
pub const MIN_INTERVAL_SECS: u64 = 1;
/// Largest accepted inter-iteration wait: 24 hours. A loop that paces slower
/// than that is a cron job.
pub const MAX_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// How often [`wait_between_iterations`] wakes to honour Ctrl+C.
const TICK: Duration = Duration::from_millis(250);

/// Parse `30s` / `5m` / `2h` into seconds, bounded by
/// [`MIN_INTERVAL_SECS`]..=[`MAX_INTERVAL_SECS`].
pub fn parse_interval(raw: &str) -> Result<u64, String> {
    let secs = crate::command::parse_repeat_interval(raw)?;
    if secs < MIN_INTERVAL_SECS {
        return Err(format!("interval must be at least {MIN_INTERVAL_SECS}s"));
    }
    if secs > MAX_INTERVAL_SECS {
        return Err(format!(
            "interval must be at most {}",
            format_secs(MAX_INTERVAL_SECS)
        ));
    }
    Ok(secs)
}

/// Render seconds the way the loop prints them (`90s`, `5m`, `2h`).
pub fn format_secs(secs: u64) -> String {
    if secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Sleep before the next iteration, waking every [`TICK`] so Ctrl+C is
/// honoured. Returns `false` when the user interrupted — the caller must stop
/// the loop with exit code 130 instead of launching another child.
pub fn wait_between_iterations(secs: u64, interrupted: &AtomicBool) -> bool {
    wait_with_tick(Duration::from_secs(secs), TICK, interrupted)
}

fn wait_with_tick(total: Duration, tick: Duration, interrupted: &AtomicBool) -> bool {
    let mut waited = Duration::ZERO;
    while waited < total {
        if interrupted.load(Ordering::SeqCst) {
            return false;
        }
        let step = (total - waited).min(tick);
        thread::sleep(step);
        waited += step;
    }
    !interrupted.load(Ordering::SeqCst)
}

/// One-line console notice printed while the loop waits.
pub fn interval_notice(secs: u64, next_iteration: u32, total: u32) -> String {
    format!(
        "[clud] waiting {} before iteration {next_iteration}/{total} (Ctrl+C stops the loop)",
        format_secs(secs)
    )
}

/// The prompt clause that turns the pacing into agent behaviour: with a real
/// wait between iterations, ending the turn is the wait, and a spin command
/// only re-pays the context.
pub fn interval_contract(secs: u64) -> String {
    let human = format_secs(secs);
    format!(
        "\nclud waits {human} between iterations. If the work you were asked for is \
not ready yet, say what you are waiting for and end your turn: the next \
iteration starts {human} from now, so ending the turn IS the wait. Do not spin \
with `true`, `sleep` or repeated polling commands — they start the next \
iteration immediately and re-pay this entire context.\n"
    )
}

#[cfg(test)]
#[path = "loop_interval_tests.rs"]
mod tests;

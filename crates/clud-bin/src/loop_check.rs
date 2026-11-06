//! DONE/BLOCKED marker checking + post-loop diagnostic output.
//! Factored out of `main.rs` so the runner module can call into it
//! cleanly without dragging the main module's whole namespace along.

use crate::command;
use crate::loop_spec;

/// Check for DONE/BLOCKED markers after an iteration finishes. Returns a
/// terminal exit code to return from the runner, or `None` to continue.
///
/// File-only variant — used by the PTY launch path, where the child writes
/// directly to the user's terminal and stdout never flows through clud.
pub fn check_loop_markers(plan: &command::LaunchPlan, iteration: u32) -> Option<i32> {
    check_loop_markers_with_output(plan, iteration, "")
}

/// Whether [`check_loop_markers_with_output`] would end the loop, without
/// printing, for a caller that must leave the alternate screen first.
pub fn markers_end_loop(plan: &command::LaunchPlan, captured_output: &str) -> bool {
    plan.loop_markers.as_ref().is_some_and(|markers| {
        let paths = loop_spec::MarkerPaths {
            done: std::path::PathBuf::from(&markers.done_path),
            blocked: std::path::PathBuf::from(&markers.blocked_path),
        };
        !matches!(
            loop_spec::read_markers_or_token(&paths, captured_output),
            loop_spec::MarkerState::None
        )
    })
}

/// Same as [`check_loop_markers`] but also scans `captured_output` for the
/// `<<<CLUD_LOOP_DONE: ...>>>` / `<<<CLUD_LOOP_BLOCKED: ...>>>` token
/// fallback (issue #95).
///
/// Marker files take precedence over tokens — if both are present, the
/// file wins. Pass an empty `captured_output` to opt out of token scanning
/// (PTY mode).
pub fn check_loop_markers_with_output(
    plan: &command::LaunchPlan,
    iteration: u32,
    captured_output: &str,
) -> Option<i32> {
    let markers = plan.loop_markers.as_ref()?;
    let paths = loop_spec::MarkerPaths {
        done: std::path::PathBuf::from(&markers.done_path),
        blocked: std::path::PathBuf::from(&markers.blocked_path),
    };
    match loop_spec::read_markers_or_token(&paths, captured_output) {
        loop_spec::MarkerState::Done(summary) => {
            if summary.is_empty() {
                eprintln!(
                    "[clud loop] DONE marker detected at iteration {iteration}; task resolved."
                );
            } else {
                eprintln!("[clud loop] DONE at iteration {iteration}: {summary}");
            }
            Some(0)
        }
        loop_spec::MarkerState::Blocked(reason) => {
            if reason.is_empty() {
                eprintln!("[clud loop] BLOCKED marker detected at iteration {iteration}; halting.");
            } else {
                eprintln!("[clud loop] BLOCKED at iteration {iteration}: {reason}");
            }
            Some(3)
        }
        loop_spec::MarkerState::None => None,
    }
}

/// Called after the iteration count is exhausted without a DONE/BLOCKED
/// marker. Only returns an override exit code when loop markers are active.
///
/// Issue #95: also print a diagnostic block listing the expected absolute
/// paths and the actual contents of `.clud/loop/`, so users can see when
/// the agent invented its own completion filename (e.g. `LOOP.md`,
/// `ITERATION_1.md`) instead of writing `DONE`.
pub fn loop_unconverged_exit(plan: &command::LaunchPlan) -> Option<i32> {
    let markers = plan.loop_markers.as_ref()?;
    eprintln!(
        "[clud loop] iteration count ({}) exhausted without a DONE marker; task did not converge.",
        plan.iterations
    );
    print_exhaustion_diagnostics(&markers.done_path, &markers.blocked_path);
    Some(2)
}

/// Print a short hint block when the loop exhausts without converging.
/// See [`loop_unconverged_exit`] for context.
fn print_exhaustion_diagnostics(done_path: &str, blocked_path: &str) {
    eprintln!("[clud loop] expected DONE marker at: {done_path}");
    eprintln!("[clud loop] expected BLOCKED marker at: {blocked_path}");

    let done_pathbuf = std::path::PathBuf::from(done_path);
    let dir = done_pathbuf
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            let names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            let listing = if names.is_empty() {
                "<empty>".to_string()
            } else {
                names.join(", ")
            };
            eprintln!("[clud loop] {} contents: {}", dir.display(), listing);

            // Issue #95: call out *.md stragglers that look like agent
            // invention (LOOP.md, ITERATION_*.md) so the user immediately
            // sees why convergence failed.
            let strays: Vec<&String> = names
                .iter()
                .filter(|n| {
                    let lower = n.to_ascii_lowercase();
                    lower.ends_with(".md") && lower != "done.md" && lower != "blocked.md"
                })
                .collect();
            if !strays.is_empty() {
                let stray_list = strays
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!(
                    "[clud loop] found *.md files NOT recognized as completion markers: {stray_list}"
                );
            }
        }
        Err(_) => {
            eprintln!("[clud loop] {} contents: <dir missing>", dir.display());
        }
    }
    eprintln!(
        "[clud loop] tip: ensure the agent writes to the exact path above. ~/.loop/ and ./LOOP.md are NOT detected."
    );
}

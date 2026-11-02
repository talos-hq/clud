//! Launch-time wiring for `clud -c` / `--last` (#922).
//!
//! Runs before the launch target is resolved. It picks a session (picker or
//! `--last`), decides how to resume it (`recover::decide`), and rewrites the
//! parsed `Args` so the ordinary plan builder does the rest: `--resume <id>`
//! for a native resume, plus `--fork-session` for a compatible provider
//! switch, or a fresh `--session-id` and a private recovery file for portable
//! recovery. When the user named no provider, the session's recorded route
//! becomes the launch's provider, so a Codex-via-Claude session resumes
//! through Codex.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::hook::RecoveryFile;
use super::import;
use super::index::{self, canonical_cwd, Route};
use super::picker::{Candidate, PickOutcome, RecoveryChoice};
use super::recover::{self, Decision, ResumeMode, ResumePlan};
use super::transcript::{estimate_resume_tokens, Transcript};
use crate::args::{Args, Command};
use crate::backend::{HarnessSelection, ModelProvider};

/// How many of the newest sessions the picker loads.
pub const MAX_CANDIDATES: usize = 30;

/// The recovery file a portable launch hands to its `SessionStart` hook.
static PENDING_RECOVERY: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Take the pending recovery file, if this launch prepared one.
pub fn take_pending_recovery() -> Option<PathBuf> {
    PENDING_RECOVERY
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
}

pub struct Environment {
    pub state_dir: PathBuf,
    pub claude_dir: PathBuf,
    pub cwd: PathBuf,
    /// stdin and stdout are both terminals.
    pub interactive: bool,
}

/// Whether this launch should go through the session picker at all.
///
/// Only a plain interactive `clud -c` (or `--last`) on the Claude harness.
/// A saved or explicit Codex/DeepSeek harness keeps its own resume; prompts
/// and subcommands keep today's pass-through.
pub fn applies(args: &Args, default_harness: Option<HarnessSelection>) -> bool {
    if !(args.continue_session || args.last) || args.dry_run {
        return false;
    }
    if args.prompt.is_some() || args.message.is_some() || args.resume.is_some() {
        return false;
    }
    if !matches!(args.command, None | Some(Command::Run)) {
        return false;
    }
    let explicit_other_harness = args.codex
        || matches!(
            args.harness,
            Some(HarnessSelection::Codex | HarnessSelection::DeepSeek)
        );
    let saved_other_harness = args.harness.is_none()
        && !args.claude
        && matches!(
            default_harness,
            Some(HarnessSelection::Codex | HarnessSelection::DeepSeek)
        );
    !explicit_other_harness && !saved_other_harness
}

/// True when the command line names a provider or routing mode.
pub fn has_explicit_provider(args: &Args) -> bool {
    args.claude
        || args.codex
        || args.deepseek
        || args.kimi
        || args.openrouter
        || args.provider.is_some()
        || args.unified
        || args.mode.is_some()
}

/// The route the command line asks for, if it names one.
pub fn requested_route(args: &Args) -> Option<Route> {
    if args.unified || args.mode.is_some() {
        return Some(super::hook::route_from_arg("unified"));
    }
    let provider = if args.claude {
        ModelProvider::Claude
    } else if args.codex {
        ModelProvider::Codex
    } else if args.deepseek {
        ModelProvider::DeepSeek
    } else if args.kimi {
        ModelProvider::Kimi
    } else if args.openrouter {
        ModelProvider::OpenRouter
    } else {
        args.provider?
    };
    Some(super::hook::route_from_arg(provider.as_str()))
}

/// Point `args` at a session's recorded route (no explicit provider given).
fn launch_on_route(args: &mut Args, route: &Route) {
    match route {
        Route::Claude => {
            args.provider = Some(ModelProvider::Claude);
            args.harness = Some(HarnessSelection::Claude);
        }
        Route::ViaClaude(name) if name == "Unified gateway" => args.unified = true,
        Route::ViaClaude(name) => {
            let provider = ModelProvider::ALL
                .iter()
                .copied()
                .find(|p| super::hook::route_from_arg(p.as_str()) == *route);
            if let Some(provider) = provider {
                args.provider = Some(provider);
                args.harness = Some(HarnessSelection::Claude);
            } else {
                eprintln!(
                    "[clud] note: session route {name} is unknown here; using the default provider"
                );
            }
        }
    }
}

/// Load the newest candidates for the picker, skipping missing transcripts.
pub fn candidates(state_dir: &Path, cwd: &str) -> Vec<Candidate> {
    index::read(state_dir, cwd)
        .newest_first()
        .into_iter()
        .filter(|entry| entry.transcript_path.is_file())
        .take(MAX_CANDIDATES)
        .filter_map(|entry| {
            let transcript = Transcript::load(&entry.transcript_path).ok()?;
            Some(Candidate {
                entry: entry.clone(),
                resume_tokens: estimate_resume_tokens(&transcript),
                checkpoints: transcript.checkpoints(),
            })
        })
        .collect()
}

/// The session to resume: the `n`th newest for `clud -c N`/`--last`
/// (#2405), otherwise the picker's choice.
fn choose(
    candidates: &[Candidate],
    direct_index: Option<usize>,
    pick: impl FnOnce(Vec<Candidate>) -> std::io::Result<PickOutcome>,
) -> Result<(String, RecoveryChoice), String> {
    if let Some(n) = direct_index {
        let chosen = candidates.get(n).ok_or_else(|| {
            format!(
                "no session {n} here: only {} earlier Claude session(s) found (0 is the newest)",
                candidates.len()
            )
        })?;
        return Ok((chosen.entry.session_id.clone(), RecoveryChoice::Full));
    }
    match pick(candidates.to_vec()).map_err(|e| format!("session picker failed: {e}"))? {
        PickOutcome::Selected { session_id, choice } => Ok((session_id, choice)),
        PickOutcome::Cancelled => Err("cancelled".to_string()),
    }
}

/// Pick, decide, and rewrite `args`. Returns a one-line note to print, or
/// `None` when the launch should keep Claude's native `--continue`.
pub fn prepare(
    args: &mut Args,
    env: &Environment,
    pick: impl FnOnce(Vec<Candidate>) -> std::io::Result<PickOutcome>,
) -> Result<Option<String>, String> {
    // Scripts: a non-TTY `clud -c` keeps Claude's native continue (#922).
    let direct = args.last || args.continue_index.is_some();
    if !env.interactive && !direct {
        return Ok(None);
    }
    if let Err(error) = import::import_once(&env.state_dir, &env.claude_dir, &env.cwd) {
        eprintln!("[clud] warning: could not import earlier sessions: {error}");
    }
    let cwd = canonical_cwd(&env.cwd);
    let candidates = candidates(&env.state_dir, &cwd);
    if candidates.is_empty() {
        if direct {
            return Err("no earlier Claude sessions were found for this directory".to_string());
        }
        return Ok(None);
    }

    let direct_index = direct.then(|| args.continue_index.unwrap_or(0));
    let (session_id, choice) = choose(&candidates, direct_index, pick)?;
    let candidate = candidates
        .iter()
        .find(|c| c.entry.session_id == session_id)
        .ok_or("the selected session disappeared")?;

    let explicit = has_explicit_provider(args);
    let requested = explicit.then(|| requested_route(args)).flatten();
    let (mode, checkpoint) = match &choice {
        RecoveryChoice::Full => (args.resume_mode, None),
        RecoveryChoice::Checkpoint(uuid) => (ResumeMode::Portable, Some(uuid.as_str())),
        RecoveryChoice::RecentHistory => (ResumeMode::Portable, None),
    };
    let window = args
        .model
        .as_deref()
        .or(candidate.entry.model.as_deref())
        .and_then(crate::server_settings::effective_context_window);
    let budget = recover::budget_for_window(window);
    let plan = recover::decide(&Decision {
        mode,
        session_id: &session_id,
        source_route: &candidate.entry.route,
        requested_route: requested.as_ref(),
        checkpoint,
        resume_tokens: candidate.resume_tokens,
        budget_tokens: budget,
    })?;

    if !explicit {
        launch_on_route(args, &candidate.entry.route);
    }
    args.continue_session = false;
    args.last = false;
    args.continue_index = None;
    let title = candidate
        .entry
        .title
        .clone()
        .unwrap_or_else(|| "untitled session".to_string());
    let route = requested.as_ref().unwrap_or(&candidate.entry.route).label();
    let note = match plan {
        ResumePlan::Native { session_id } => {
            args.resume = Some(Some(session_id));
            format!("[clud] resuming \"{title}\" ({route})")
        }
        ResumePlan::ForkedNative { session_id } => {
            args.resume = Some(Some(session_id));
            args.passthrough.push("--fork-session".to_string());
            format!("[clud] resuming \"{title}\" as a fork on {route}")
        }
        ResumePlan::Portable {
            from_session,
            checkpoint,
        } => {
            let transcript = Transcript::load(&candidate.entry.transcript_path)
                .map_err(|e| format!("could not read the selected transcript: {e}"))?;
            let recovery =
                recover::build_recovery(&transcript, &from_session, checkpoint.as_deref(), budget)?;
            let new_id = super::new_session_uuid()?;
            let path = index::index_dir(&env.state_dir)
                .join("recovery")
                .join(format!("{new_id}.json"));
            let file = RecoveryFile {
                context: recovery.context,
                lineage: recovery.lineage,
            };
            let bytes = serde_json::to_vec(&file).map_err(|e| e.to_string())?;
            crate::fs_private::write_private_atomic(&path, &bytes)
                .map_err(|e| format!("could not write the recovery file: {e}"))?;
            if let Ok(mut slot) = PENDING_RECOVERY.lock() {
                *slot = Some(path);
            }
            args.passthrough.push("--session-id".to_string());
            args.passthrough.push(new_id);
            format!(
                "[clud] recovering \"{title}\" into a new session on {route} (~{} tokens of history, truncated)",
                crate::session_history::picker::short_tokens(recovery.tokens)
            )
        }
    };
    Ok(Some(note))
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;

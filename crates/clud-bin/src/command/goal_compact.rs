//! Fit a long `clud do` brief into Claude Code's `/goal` condition (#2522).
//!
//! Claude Code rejects a condition over [`CLAUDE_GOAL_CONDITION_LIMIT`]
//! characters. A brief that fits is quoted whole. A longer one is condensed
//! by a language model, and the result is accepted only when every hard
//! constraint of the brief survives verbatim: a clause naming a path, or one
//! that forbids or requires something ("do not touch", "never push", "must",
//! acceptance criteria). When the model is unavailable or drops a
//! constraint, the goal falls back to the constraints themselves, quoted
//! verbatim. When even those cannot fit, compaction fails and quotes the
//! clause that would have been lost; nothing is ever truncated silently.
//!
//! The full brief always reaches the agent in the appended system prompt and
//! on disk (see `record`), so the compacted goal points at it.

use std::fmt;
use std::path::{Path, PathBuf};

use super::prompts::CLAUDE_GOAL_CONDITION_LIMIT;

/// Phrases that make a clause a hard constraint, matched case-insensitively.
const CONSTRAINT_MARKERS: &[&str] = &[
    "do not",
    "don't",
    "never",
    "must",
    "only",
    "no ",
    "without",
    "acceptance",
    "required",
    "forbid",
    "keep ",
];

/// File extensions that make a bare `name.ext` token a path.
const PATH_EXTENSIONS: &[&str] = &[
    "rs", "py", "md", "toml", "json", "yml", "yaml", "sh", "ts", "js", "txt", "lock",
];

/// Why a brief could not be compacted into the cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactError {
    /// The verbatim hard constraints alone exceed the cap; `clause` is the
    /// first one that no longer fits.
    ConstraintDoesNotFit { clause: String, limit: usize },
}

impl fmt::Display for CompactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConstraintDoesNotFit { clause, limit } => write!(
                f,
                "the /goal condition cannot fit in {limit} characters without dropping the \
hard constraint \"{clause}\"; shorten the brief's constraints and retry"
            ),
        }
    }
}

/// How the goal condition was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactKind {
    /// The whole brief fit and is quoted unchanged.
    Verbatim,
    /// A language model condensed the brief; every constraint survived.
    Model,
    /// The model was unavailable or dropped a constraint; the condition
    /// lists the constraints verbatim.
    Constraints,
}

/// A goal condition (without the `/goal ` prefix) that fits the cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactGoal {
    pub condition: String,
    pub kind: CompactKind,
}

/// Normalize for comparison: lowercase, single spaces.
fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Split a brief into clauses at line breaks, `;` and sentence ends.
fn clauses(brief: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in brief.lines() {
        for part in line.split(';') {
            let mut rest = part;
            while let Some(at) = rest.find(". ") {
                out.push(rest[..=at].to_string());
                rest = &rest[at + 2..];
            }
            out.push(rest.to_string());
        }
    }
    out.into_iter()
        .map(|clause| {
            clause
                .trim()
                .trim_start_matches(['-', '*', ' '])
                .trim()
                .to_string()
        })
        .filter(|clause| !clause.is_empty())
        .collect()
}

fn is_path_token(token: &str) -> bool {
    let token = token.trim_matches(|c: char| "`'\"(),:;".contains(c));
    let token = token.trim_end_matches('.');
    if token.len() < 2 || token.contains("://") {
        return false;
    }
    if token.starts_with('~') || token.starts_with("./") || token.starts_with("../") {
        return true;
    }
    if token.starts_with('/') && token[1..].chars().next().is_some_and(char::is_alphanumeric) {
        return true;
    }
    if token.contains('/') && token.split('/').all(|seg| !seg.is_empty()) {
        // `crates/clud-bin/src` style relative paths; not `and/or` prose.
        return token.contains('.') || token.contains('_') || token.contains('-');
    }
    token
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && PATH_EXTENSIONS.contains(&ext))
}

/// Every path-looking token in `text`, in order, without duplicates.
pub(crate) fn path_tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split_whitespace() {
        if !is_path_token(raw) {
            continue;
        }
        let token = raw
            .trim_matches(|c: char| "`'\"(),:;".contains(c))
            .trim_end_matches('.')
            .to_string();
        if !out.contains(&token) {
            out.push(token);
        }
    }
    out
}

/// The brief's hard-constraint clauses, verbatim, in order.
pub(crate) fn hard_constraints(brief: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for clause in clauses(brief) {
        let lower = clause.to_lowercase();
        let marked = CONSTRAINT_MARKERS.iter().any(|m| lower.contains(m));
        if (marked || !path_tokens(&clause).is_empty()) && !out.contains(&clause) {
            out.push(clause);
        }
    }
    out
}

/// Whether `condition` keeps every constraint verbatim (whitespace and case
/// insensitive).
pub(crate) fn keeps_constraints(condition: &str, constraints: &[String]) -> bool {
    let haystack = normalized(condition);
    constraints
        .iter()
        .all(|clause| haystack.contains(&normalized(clause)))
}

/// The instruction handed to the language model.
pub(crate) fn model_prompt(brief: &str, budget: usize, constraints: &[String]) -> String {
    let mut prompt = format!(
        "Condense the task brief below into a goal statement of at most {budget} characters. \
Copy every one of the following hard-constraint clauses verbatim, character for character, \
and compress only the remaining prose. Output only the goal statement, with no preamble.\n\n\
Hard constraints:\n"
    );
    for clause in constraints {
        prompt.push_str("- ");
        prompt.push_str(clause);
        prompt.push('\n');
    }
    prompt.push_str("\nBrief:\n");
    prompt.push_str(brief);
    prompt
}

/// Fit `brief` after `preamble` into the cap.
///
/// `lm` receives the model prompt and returns the model's answer; it is only
/// called when the brief does not fit verbatim.
pub(crate) fn compact_goal(
    preamble: &str,
    brief: &str,
    lm: impl FnOnce(&str) -> Result<String, String>,
) -> Result<CompactGoal, CompactError> {
    let limit = CLAUDE_GOAL_CONDITION_LIMIT;
    let verbatim = format!("{preamble}{brief}");
    if verbatim.chars().count() <= limit {
        return Ok(CompactGoal {
            condition: verbatim,
            kind: CompactKind::Verbatim,
        });
    }
    let constraints = hard_constraints(brief);
    let budget = limit.saturating_sub(preamble.chars().count());
    if let Ok(answer) = lm(&model_prompt(brief, budget, &constraints)) {
        let condition = format!("{preamble}{}", answer.trim());
        if condition.chars().count() <= limit && keeps_constraints(&condition, &constraints) {
            return Ok(CompactGoal {
                condition,
                kind: CompactKind::Model,
            });
        }
    }
    constraints_only(preamble, &constraints, limit)
}

fn constraints_only(
    preamble: &str,
    constraints: &[String],
    limit: usize,
) -> Result<CompactGoal, CompactError> {
    let mut condition =
        format!("{preamble}(compacted; the full target is in your system prompt) Hard constraints, verbatim:");
    for clause in constraints {
        let next = format!("{condition} {clause}");
        if next.chars().count() > limit {
            return Err(CompactError::ConstraintDoesNotFit {
                clause: clause.clone(),
                limit,
            });
        }
        condition = next;
    }
    Ok(CompactGoal {
        condition,
        kind: CompactKind::Constraints,
    })
}

/// Where a session's brief and exercised goal are recorded:
/// `~/.clud/state/do-goals/<session-id>/`, keyed by the same id as the
/// session's checkout claim.
pub(crate) fn session_record_dir(home: &Path, session_id: &str) -> PathBuf {
    home.join(".clud")
        .join("state")
        .join("do-goals")
        .join(session_id)
}

/// Exported to the harness: the recorded `goal.txt` of this session.
pub const GOAL_RECORD_ENV: &str = "CLUD_DO_GOAL_FILE";

/// Write the original brief and the exercised goal; returns the brief path.
pub(crate) fn record(dir: &Path, brief: &str, goal: &CompactGoal) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let brief_path = dir.join("brief.md");
    std::fs::write(&brief_path, brief)?;
    std::fs::write(dir.join("goal.txt"), &goal.condition)?;
    Ok(brief_path)
}

/// Env var naming the model `claude -p` uses to compact a goal.
pub(crate) const COMPACT_MODEL_ENV: &str = "CLUD_GOAL_COMPACT_MODEL";
const DEFAULT_COMPACT_MODEL: &str = "haiku";
const COMPACT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);

/// Ask the configured model (`claude -p`) to condense a brief.
fn run_model(prompt: &str) -> Result<String, String> {
    let model = std::env::var(COMPACT_MODEL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_COMPACT_MODEL.to_string());
    let argv = vec![
        "claude".to_string(),
        "-p".to_string(),
        "--model".to_string(),
        model,
        prompt.to_string(),
    ];
    let (code, out) =
        crate::backend_bootstrap::run_captured_command_with_deadline(argv, COMPACT_DEADLINE)?;
    if code != 0 {
        return Err(format!("claude -p exited {code}"));
    }
    Ok(out)
}

/// A compacted goal ready for launch, and where it was recorded.
pub struct PreparedGoal {
    pub condition: String,
    pub record: Option<PathBuf>,
}

/// Before launch: the exercised Claude `/do` goal for `target`, or `None`
/// when the target fits the cap verbatim. A compacted goal is recorded with
/// the original brief under the session's record dir and echoed to stderr.
pub fn prepare_for_launch(target: &str) -> Result<Option<PreparedGoal>, CompactError> {
    let plain = super::prompts::do_goal_preamble(None);
    if plain.chars().count() + target.chars().count() <= CLAUDE_GOAL_CONDITION_LIMIT {
        return Ok(None);
    }
    let session_id = crate::daemon::foreground_session_id();
    let dir = crate::home::user_home().map(|home| session_record_dir(&home, &session_id));
    let brief_path = dir.as_ref().map(|dir| dir.join("brief.md"));
    let preamble = super::prompts::do_goal_preamble(
        brief_path
            .as_deref()
            .map(|p| p.to_str().unwrap_or_default()),
    );
    let goal = compact_goal(&preamble, target, run_model)?;
    let record = dir
        .as_ref()
        .and_then(|dir| match record(dir, target, &goal) {
            Ok(_) => Some(dir.join("goal.txt")),
            Err(error) => {
                eprintln!("[clud] warning: could not record the compacted goal: {error}");
                None
            }
        });
    eprintln!(
        "[clud] /goal compacted from {} to {} characters ({:?}) for session {session_id}; \
exercised goal:\n{}",
        target.chars().count(),
        goal.condition.chars().count(),
        goal.kind,
        goal.condition
    );
    Ok(Some(PreparedGoal {
        condition: goal.condition,
        record,
    }))
}

#[cfg(test)]
#[path = "goal_compact_tests.rs"]
mod tests;

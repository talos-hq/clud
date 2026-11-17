//! The interactive `clud -c` session picker (#922).
//!
//! Lists this cwd's Claude-harness sessions, newest first. Selecting a
//! session with compact checkpoints opens a second step to choose how to
//! resume it. Rendering and input belong to `selector::run` (DD-073); this
//! module only supplies views and reacts to keys.

use std::io::{self, Write};
use std::time::Duration;

use super::index::SessionEntry;
use super::transcript::Checkpoint;
use crate::selector::{self, check_marker, Key, Note, OnExit, Row, Selector, Step, View};

/// A session the picker can offer, with what the rows need to show.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub entry: SessionEntry,
    pub resume_tokens: u64,
    pub checkpoints: Vec<Checkpoint>,
}

/// How to resume the chosen session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryChoice {
    /// The whole session, per `--resume-mode` (auto by default).
    Full,
    /// Portable recovery starting from this compact checkpoint.
    Checkpoint(String),
    /// Portable recovery from the newest checkpoint and recent turns.
    RecentHistory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickOutcome {
    Selected {
        session_id: String,
        choice: RecoveryChoice,
    },
    Cancelled,
}

enum Stage {
    Sessions,
    /// Recovery choices for `candidates[session]`.
    Recovery {
        session: usize,
        selected: usize,
    },
}

pub struct SessionPicker {
    candidates: Vec<Candidate>,
    selected: usize,
    stage: Stage,
}

/// `2026-09-25 06:20` from an RFC 3339 timestamp; the raw value otherwise.
fn short_time(timestamp: Option<&str>) -> String {
    let Some(timestamp) = timestamp else {
        return "unknown time".to_string();
    };
    match (timestamp.get(0..10), timestamp.get(11..16)) {
        (Some(date), Some(time)) => format!("{date} {time}"),
        _ => timestamp.to_string(),
    }
}

/// `12k`, `1.2M`: compact token counts for a row.
pub fn short_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

impl SessionPicker {
    pub fn new(candidates: Vec<Candidate>) -> Self {
        Self {
            candidates,
            selected: 0,
            stage: Stage::Sessions,
        }
    }

    fn recovery_options(candidate: &Candidate) -> Vec<(String, RecoveryChoice)> {
        let mut options = vec![("Full session".to_string(), RecoveryChoice::Full)];
        for (n, checkpoint) in candidate.checkpoints.iter().enumerate().rev() {
            options.push((
                format!(
                    "Compact checkpoint {} ({})",
                    n + 1,
                    short_time(checkpoint.timestamp.as_deref())
                ),
                RecoveryChoice::Checkpoint(checkpoint.uuid.clone()),
            ));
        }
        options.push((
            "Recent-history recovery".to_string(),
            RecoveryChoice::RecentHistory,
        ));
        options
    }

    fn session_row(&self, index: usize, candidate: &Candidate) -> Row {
        let entry = &candidate.entry;
        let current = index == self.selected;
        let compact = match candidate.checkpoints.len() {
            0 => String::new(),
            1 => " · 1 checkpoint".to_string(),
            n => format!(" · {n} checkpoints"),
        };
        Row {
            current,
            marker: check_marker(current).to_string(),
            label: entry
                .title
                .clone()
                .unwrap_or_else(|| "(untitled session)".to_string()),
            note: Note::Below(format!(
                "{} · {} · ~{} tokens{compact}",
                short_time(entry.last_activity.as_deref()),
                entry.route.label(),
                short_tokens(candidate.resume_tokens)
            )),
        }
    }
}

impl Selector for SessionPicker {
    type Outcome = PickOutcome;

    fn view(&self, _elapsed: Duration) -> View {
        match &self.stage {
            Stage::Sessions => View {
                title: "Continue a session in this directory".to_string(),
                hints: vec!["Up/Down choose, Enter select, Esc cancel".to_string()],
                gap: true,
                rows: self
                    .candidates
                    .iter()
                    .enumerate()
                    .map(|(i, c)| self.session_row(i, c))
                    .collect(),
                footer: Vec::new(),
                ..View::default()
            },
            Stage::Recovery { session, selected } => {
                let candidate = &self.candidates[*session];
                View {
                    title: format!(
                        "Resume \"{}\"",
                        candidate.entry.title.as_deref().unwrap_or("(untitled session)")
                    ),
                    hints: vec!["Up/Down choose, Enter resume, Esc back".to_string()],
                    gap: true,
                    rows: Self::recovery_options(candidate)
                        .into_iter()
                        .enumerate()
                        .map(|(i, (label, _))| Row {
                            current: i == *selected,
                            marker: check_marker(i == *selected).to_string(),
                            label,
                            note: Note::None,
                        })
                        .collect(),
                    footer: vec![
                        "Checkpoint and recent-history choices start a new session with a truncated history"
                            .to_string(),
                    ],
                    ..View::default()
                }
            }
        }
    }

    fn on_key(&mut self, key: Key) -> Step<PickOutcome> {
        match &mut self.stage {
            Stage::Sessions => match key {
                Key::Up => {
                    self.selected = self.selected.saturating_sub(1);
                    Step::Redraw
                }
                Key::Down => {
                    if self.selected + 1 < self.candidates.len() {
                        self.selected += 1;
                    }
                    Step::Redraw
                }
                Key::Enter => {
                    let candidate = &self.candidates[self.selected];
                    if candidate.checkpoints.is_empty() {
                        Step::Done(PickOutcome::Selected {
                            session_id: candidate.entry.session_id.clone(),
                            choice: RecoveryChoice::Full,
                        })
                    } else {
                        self.stage = Stage::Recovery {
                            session: self.selected,
                            selected: 0,
                        };
                        Step::Redraw
                    }
                }
                Key::Escape => Step::Done(PickOutcome::Cancelled),
                Key::Space | Key::Char(_) => Step::Stay,
            },
            Stage::Recovery { session, selected } => {
                let options = Self::recovery_options(&self.candidates[*session]);
                match key {
                    Key::Up => {
                        *selected = selected.saturating_sub(1);
                        Step::Redraw
                    }
                    Key::Down => {
                        if *selected + 1 < options.len() {
                            *selected += 1;
                        }
                        Step::Redraw
                    }
                    Key::Enter => Step::Done(PickOutcome::Selected {
                        session_id: self.candidates[*session].entry.session_id.clone(),
                        choice: options[*selected].1.clone(),
                    }),
                    Key::Escape => {
                        self.stage = Stage::Sessions;
                        Step::Redraw
                    }
                    Key::Space | Key::Char(_) => Step::Stay,
                }
            }
        }
    }

    fn on_exit(&self) -> OnExit {
        OnExit::Erase
    }
}

/// Show the picker until a session is chosen or the user cancels.
pub fn prompt<W: Write>(out: &mut W, candidates: Vec<Candidate>) -> io::Result<PickOutcome> {
    let mut picker = SessionPicker::new(candidates);
    match selector::run(out, &mut picker) {
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(PickOutcome::Cancelled),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_history::index::tests::entry;
    use crate::session_history::index::Route;

    fn candidate(id: &str, title: &str, checkpoints: usize) -> Candidate {
        let mut e = entry(
            id,
            "2026-09-25T06:20:00Z",
            Route::ViaClaude("Codex".into()),
            false,
        );
        e.title = Some(title.into());
        Candidate {
            entry: e,
            resume_tokens: 12_345,
            checkpoints: (0..checkpoints)
                .map(|n| Checkpoint {
                    uuid: format!("c{n}"),
                    position: n,
                    timestamp: Some(format!("2026-09-2{n}T01:00:00Z")),
                    summary: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn rows_show_title_time_route_estimate_and_checkpoints() {
        let picker = SessionPicker::new(vec![candidate("a", "Fix flaky test", 2)]);
        let view = picker.view(Duration::ZERO);
        assert_eq!(view.rows[0].label, "Fix flaky test");
        assert_eq!(
            view.rows[0].note,
            Note::Below("2026-09-25 06:20 · Codex via Claude · ~12k tokens · 2 checkpoints".into())
        );
    }

    #[test]
    fn a_session_without_checkpoints_resumes_in_full_directly() {
        let mut picker = SessionPicker::new(vec![candidate("a", "A", 0), candidate("b", "B", 0)]);
        assert_eq!(picker.on_key(Key::Down), Step::Redraw);
        assert_eq!(
            picker.on_key(Key::Enter),
            Step::Done(PickOutcome::Selected {
                session_id: "b".into(),
                choice: RecoveryChoice::Full
            })
        );
    }

    #[test]
    fn checkpoints_open_a_recovery_step_newest_checkpoint_first() {
        let mut picker = SessionPicker::new(vec![candidate("a", "A", 2)]);
        assert_eq!(picker.on_key(Key::Enter), Step::Redraw);
        let labels: Vec<String> = picker
            .view(Duration::ZERO)
            .rows
            .into_iter()
            .map(|r| r.label)
            .collect();
        assert_eq!(labels[0], "Full session");
        assert!(labels[1].starts_with("Compact checkpoint 2"));
        assert!(labels[2].starts_with("Compact checkpoint 1"));
        assert_eq!(labels[3], "Recent-history recovery");
        picker.on_key(Key::Down);
        assert_eq!(
            picker.on_key(Key::Enter),
            Step::Done(PickOutcome::Selected {
                session_id: "a".into(),
                choice: RecoveryChoice::Checkpoint("c1".into())
            })
        );
    }

    #[test]
    fn escape_backs_out_of_the_recovery_step_then_cancels() {
        let mut picker = SessionPicker::new(vec![candidate("a", "A", 1)]);
        picker.on_key(Key::Enter);
        assert_eq!(picker.on_key(Key::Escape), Step::Redraw);
        assert_eq!(
            picker.view(Duration::ZERO).title,
            "Continue a session in this directory"
        );
        assert_eq!(
            picker.on_key(Key::Escape),
            Step::Done(PickOutcome::Cancelled)
        );
    }

    #[test]
    fn token_counts_are_compact() {
        assert_eq!(short_tokens(999), "999");
        assert_eq!(short_tokens(12_345), "12k");
        assert_eq!(short_tokens(1_234_567), "1.2M");
    }
}

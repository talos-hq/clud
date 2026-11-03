//! Restart a finished `clud loop` from the terminal (#2471).
//!
//! When an interactive loop ends, clud offers "Restart loop" / "Exit"
//! through the shared [`crate::selector`] (this module never drives the
//! terminal itself). Restarting archives the previous run's artifacts under
//! `.clud/loop/history/<stamp>/`, clears the DONE/BLOCKED markers, and runs
//! the same command line again, so queued `--message`s, `loop.json` and the
//! resolved task are picked up exactly as on the first run.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::loop_spec::loop_dir;
use crate::selector::{check_marker, Key, Note, Row, Selector, Step, View};

/// What the operator chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Restart,
    Exit,
}

/// The two-row restart prompt.
pub struct RestartPrompt {
    summary: String,
    current: usize,
}

impl RestartPrompt {
    pub fn new(summary: &str) -> Self {
        Self {
            summary: summary.to_string(),
            current: 0,
        }
    }
}

impl Selector for RestartPrompt {
    type Outcome = Choice;

    fn view(&self, _elapsed: Duration) -> View {
        let rows = [
            ("Restart loop", "archive this run, clear markers, run again"),
            ("Exit", "keep LOOP_DONE.md and stop"),
        ]
        .iter()
        .enumerate()
        .map(|(i, (label, note))| Row {
            current: i == self.current,
            marker: check_marker(i == self.current).to_string(),
            label: (*label).to_string(),
            note: Note::Inline((*note).to_string()),
        })
        .collect();
        View {
            title: format!("Loop finished: {}", self.summary),
            hints: vec!["Up/Down to choose, Enter to confirm, r restarts, q exits".into()],
            gap: true,
            rows,
            footer: Vec::new(),
        }
    }

    fn on_key(&mut self, key: Key) -> Step<Choice> {
        match key {
            Key::Up | Key::Down => {
                self.current = 1 - self.current;
                Step::Redraw
            }
            Key::Enter | Key::Space => Step::Done(if self.current == 0 {
                Choice::Restart
            } else {
                Choice::Exit
            }),
            Key::Char('r') => Step::Done(Choice::Restart),
            Key::Char('q') | Key::Escape => Step::Done(Choice::Exit),
            _ => Step::Stay,
        }
    }
}

/// Offer a restart only on an interactive terminal outside `--dry-run`.
pub fn should_offer(stdin_tty: bool, stderr_tty: bool, dry_run: bool) -> bool {
    stdin_tty && stderr_tty && !dry_run
}

/// Files moved aside on restart. `loop.json` and `inbox/` stay: they are
/// inputs for the next run, not results of the last one.
const RUN_ARTIFACTS: &[&str] = &["info.json", "log.txt", "NEXT_MESSAGE.md", "motivation.md"];

/// Archive the previous run and clear its markers. Returns the history dir.
pub fn archive_run(git_root: &Path, done: &Path, blocked: &Path) -> std::io::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let history = loop_dir(git_root).join("history").join(stamp.to_string());
    std::fs::create_dir_all(&history)?;
    let root_summary = crate::loop_artifacts::root_summary_path(git_root);
    let mut moves: Vec<(PathBuf, String)> = RUN_ARTIFACTS
        .iter()
        .map(|n| (loop_dir(git_root).join(n), (*n).to_string()))
        .collect();
    moves.push((root_summary, "LOOP_DONE.md".into()));
    moves.push((done.to_path_buf(), "DONE".into()));
    moves.push((blocked.to_path_buf(), "BLOCKED".into()));
    for (src, name) in moves {
        if src.is_file() {
            std::fs::rename(&src, history.join(name))?;
        }
    }
    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_choose_restart_or_exit() {
        let mut p = RestartPrompt::new("DONE");
        assert_eq!(p.on_key(Key::Enter), Step::Done(Choice::Restart));
        let mut p = RestartPrompt::new("DONE");
        assert_eq!(p.on_key(Key::Down), Step::Redraw);
        assert_eq!(p.on_key(Key::Enter), Step::Done(Choice::Exit));
        assert_eq!(
            RestartPrompt::new("x").on_key(Key::Char('r')),
            Step::Done(Choice::Restart)
        );
        assert_eq!(
            RestartPrompt::new("x").on_key(Key::Char('q')),
            Step::Done(Choice::Exit)
        );
        assert_eq!(
            RestartPrompt::new("x").on_key(Key::Escape),
            Step::Done(Choice::Exit)
        );
        assert_eq!(RestartPrompt::new("x").on_key(Key::Char('z')), Step::Stay);
    }

    #[test]
    fn view_names_outcome_and_highlights_current_row() {
        let mut p = RestartPrompt::new("BLOCKED: needs creds");
        let v = p.view(Duration::ZERO);
        assert_eq!(v.title, "Loop finished: BLOCKED: needs creds");
        assert!(v.rows[0].current && !v.rows[1].current);
        p.on_key(Key::Down);
        let v = p.view(Duration::ZERO);
        assert!(!v.rows[0].current && v.rows[1].current);
        assert_eq!(v.rows[1].label, "Exit");
    }

    #[test]
    fn offer_requires_interactive_terminal() {
        assert!(should_offer(true, true, false));
        assert!(!should_offer(false, true, false));
        assert!(!should_offer(true, false, false));
        assert!(!should_offer(true, true, true));
    }

    #[test]
    fn archive_moves_results_keeps_inputs_and_clears_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = loop_dir(root);
        std::fs::create_dir_all(dir.join("inbox")).unwrap();
        for f in ["info.json", "log.txt", "DONE", "loop.json"] {
            std::fs::write(dir.join(f), f).unwrap();
        }
        std::fs::write(dir.join("inbox").join("m.md"), "next").unwrap();
        std::fs::write(root.join("LOOP_DONE.md"), "summary").unwrap();

        let history = archive_run(root, &dir.join("DONE"), &dir.join("BLOCKED")).unwrap();

        for f in ["info.json", "log.txt", "DONE", "LOOP_DONE.md"] {
            assert!(history.join(f).is_file(), "{f} not archived");
        }
        assert!(!dir.join("DONE").exists());
        assert!(!dir.join("info.json").exists());
        assert!(!root.join("LOOP_DONE.md").exists());
        assert!(dir.join("loop.json").is_file(), "loop.json is an input");
        assert!(
            dir.join("inbox").join("m.md").is_file(),
            "queued messages stay"
        );
        assert!(history.starts_with(dir.join("history")));
    }
}

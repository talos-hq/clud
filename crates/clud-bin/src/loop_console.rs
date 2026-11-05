//! Interactive dashboard for headless loop children. Never runs beside a child TUI.
use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::{cursor, execute, queue, style, terminal};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Watching,
    Queue,
    Interrupt,
    Confirm,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Queue(String),
    Restart(String),
    Quit,
}

#[derive(Default)]
struct Dashboard {
    mode: Mode,
    draft: String,
    notice: String,
    lines: VecDeque<String>,
    scroll: usize,
    page: usize,
}

impl Dashboard {
    fn event(&mut self, event: Event) -> Option<Action> {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_up(3),
                    MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(3),
                    _ => {}
                }
                None
            }
            Event::Paste(text) if matches!(self.mode, Mode::Queue | Mode::Interrupt) => {
                self.draft
                    .extend(text.chars().filter(|c| !c.is_control() || *c == '\n'));
                None
            }
            _ => None,
        }
    }

    fn key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Action::Quit);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        if key.code == KeyCode::Esc {
            self.mode = Mode::Watching;
            self.draft.clear();
            return None;
        }
        match key.code {
            KeyCode::PageUp => self.scroll_up(self.page.max(1)),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.page.max(1)),
            KeyCode::Home if self.mode == Mode::Watching => self.scroll_up(self.lines.len()),
            KeyCode::End if self.mode == Mode::Watching => self.scroll = 0,
            KeyCode::Up if self.mode == Mode::Watching => self.scroll_up(1),
            KeyCode::Down if self.mode == Mode::Watching => {
                self.scroll = self.scroll.saturating_sub(1)
            }
            _ => return self.mode_key(key),
        }
        None
    }

    fn scroll_up(&mut self, count: usize) {
        self.scroll = self
            .scroll
            .saturating_add(count)
            .min(self.lines.len().saturating_sub(self.page.max(1)));
    }

    fn mode_key(&mut self, key: KeyEvent) -> Option<Action> {
        match self.mode {
            Mode::Watching => match key.code {
                KeyCode::Char('q') => {
                    self.mode = Mode::Queue;
                    self.notice.clear();
                }
                KeyCode::Char('i') => {
                    self.mode = Mode::Interrupt;
                    self.notice.clear();
                }
                _ => {}
            },
            Mode::Confirm => match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.mode = Mode::Watching;
                    return Some(Action::Restart(std::mem::take(&mut self.draft)));
                }
                KeyCode::Char('n' | 'N') | KeyCode::Enter => self.mode = Mode::Interrupt,
                _ => {}
            },
            Mode::Queue | Mode::Interrupt => return self.edit(key),
        }
        None
    }

    fn edit(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Enter if !self.draft.trim().is_empty() => {
                self.notice.clear();
                if self.mode == Mode::Interrupt {
                    self.mode = Mode::Confirm;
                } else {
                    self.mode = Mode::Watching;
                    return Some(Action::Queue(std::mem::take(&mut self.draft)));
                }
            }
            KeyCode::Backspace => {
                self.draft.pop();
            }
            KeyCode::Char(c)
                if !c.is_control()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.draft.push(c);
            }
            _ => {}
        }
        None
    }

    fn push(&mut self, line: &str) {
        for line in line.lines() {
            self.lines.push_back(line.to_owned());
            if self.scroll > 0 {
                self.scroll = self.scroll.saturating_add(1);
            }
        }
        while self.lines.len() > 10_000 {
            self.lines.pop_front();
        }
        self.scroll = self
            .scroll
            .min(self.lines.len().saturating_sub(self.page.max(1)));
    }

    fn save(&mut self, root: &Path, text: &str, retry_mode: Mode) -> bool {
        match crate::loop_artifacts::queue_message(root, text) {
            Ok(_) => {
                self.notice = "Message queued for next iteration".to_owned();
                true
            }
            Err(error) => {
                self.notice = format!("Could not queue message: {error}");
                self.draft = text.to_owned();
                self.mode = retry_mode;
                false
            }
        }
    }

    fn footer(&self) -> String {
        if self.mode != Mode::Watching && !self.notice.is_empty() {
            return format!("{} | Enter: retry  Esc: cancel", self.notice);
        }
        match self.mode {
            Mode::Watching => format!(
                "[q] Queue message   [i] Interrupt with message   [Ctrl+C] Stop | {}",
                self.notice
            ),
            Mode::Queue => "Queue message (next iteration) | Enter: save  Esc: cancel".to_owned(),
            Mode::Interrupt => "Interrupt with message | Enter: review  Esc: cancel".to_owned(),
            Mode::Confirm => {
                "This will restart the loop. Are you sure? [y] Yes [Enter/n] Edit [Esc] Cancel"
                    .to_owned()
            }
        }
    }

    fn draw(
        &self,
        out: &mut impl Write,
        cols: u16,
        rows: u16,
        iteration: u32,
        total: u32,
    ) -> io::Result<()> {
        let header = format!("clud loop | iteration {iteration}/{total}");
        queue!(out, style::SetForegroundColor(style::Color::Cyan))?;
        write_row(out, 0, cols, &header)?;
        queue!(out, style::ResetColor)?;
        let available = usize::from(rows.saturating_sub(3));
        let end = self.lines.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(available);
        for n in 0..available {
            let line = self.lines.get(start + n).filter(|_| start + n < end);
            write_row(
                out,
                u16::try_from(n + 1).unwrap_or(u16::MAX),
                cols,
                line.map_or("", String::as_str),
            )?;
        }
        if rows >= 2 {
            let editor = if self.mode == Mode::Watching {
                format!("PgUp/PgDn, arrows, wheel: scroll | Home: oldest End: live | {} lines above live", self.scroll)
            } else {
                let tail: String = self
                    .draft
                    .chars()
                    .rev()
                    .take(usize::from(cols.saturating_sub(4)) / 2)
                    .collect();
                format!(
                    "> {}",
                    tail.chars().rev().collect::<String>().replace('\n', " / ")
                )
            };
            write_row(out, rows - 2, cols, &editor)?;
        }
        queue!(out, style::SetForegroundColor(style::Color::Cyan))?;
        write_row(out, rows.saturating_sub(1), cols, &self.footer())?;
        queue!(out, style::ResetColor)?;
        out.flush()
    }
}

// vt100 owns display-cell measurement, including wide and combining characters.
// Feeding only printable characters also prevents child output/pastes from injecting VT commands.
fn write_row(out: &mut impl Write, row: u16, cols: u16, text: &str) -> io::Result<()> {
    queue!(out, cursor::MoveTo(0, row))?;
    let mut screen = vt100::Parser::new(2, cols.max(1), 0);
    for c in text.chars().filter(|c| !c.is_control()) {
        let mut buf = [0; 4];
        let bytes = c.encode_utf8(&mut buf).as_bytes();
        screen.process(bytes);
        let (line, col) = screen.screen().cursor_position();
        if line > 0 || col >= cols.saturating_sub(1) {
            break;
        }
        out.write_all(bytes)?;
    }
    queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    Ok(())
}

#[derive(Default)]
struct Frame {
    previous: Option<vt100::Screen>,
}

impl Frame {
    fn update(&mut self, bytes: &[u8], cols: u16, rows: u16) -> Vec<u8> {
        let mut parser = vt100::Parser::new(rows.max(1), cols.max(1), 0);
        parser.process(b"\x1b[?25l");
        parser.process(bytes);
        let screen = parser.screen();
        let output = match &self.previous {
            Some(previous) if previous.size() == screen.size() => screen.contents_diff(previous),
            _ => screen.contents_formatted(),
        };
        self.previous = Some(screen.clone());
        output
    }
}

pub(crate) struct LoopConsole {
    root: PathBuf,
    dashboard: Dashboard,
    iteration: u32,
    total: u32,
    frame: Frame,
}

impl LoopConsole {
    pub(crate) fn enter(root: &Path, iteration: u32, total: u32) -> io::Result<Option<Self>> {
        if !crate::session::terminals_are_interactive()
            || !io::stderr().is_terminal()
            || std::env::var("TERM").is_ok_and(|s| s == "dumb")
        {
            return Ok(None);
        }
        terminal::enable_raw_mode()?;
        let mut console = Self {
            root: root.to_owned(),
            dashboard: Dashboard::default(),
            iteration,
            total,
            frame: Frame::default(),
        };
        // Construct the guard first so a partial setup failure still restores the terminal.
        execute!(
            io::stderr(),
            terminal::EnterAlternateScreen,
            cursor::Hide,
            crossterm::event::EnableMouseCapture,
            crossterm::event::EnableBracketedPaste
        )?;
        console.draw()?;
        Ok(Some(console))
    }

    pub(crate) fn transcript(&self) -> String {
        self.dashboard
            .lines
            .iter()
            .map(|line| line.chars().filter(|c| !c.is_control()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn line(&mut self, line: &str) -> io::Result<()> {
        self.dashboard.push(line);
        self.draw()
    }

    pub(crate) fn input(&mut self) -> io::Result<Option<Action>> {
        if !crossterm::event::poll(std::time::Duration::ZERO)? {
            return Ok(None);
        }
        let action = self.dashboard.event(crossterm::event::read()?);
        let result = match action {
            Some(Action::Queue(text)) => {
                self.dashboard.save(&self.root, &text, Mode::Queue);
                None
            }
            Some(Action::Restart(text)) => self
                .dashboard
                .save(&self.root, &text, Mode::Interrupt)
                .then_some(Action::Restart(text)),
            other => other,
        };
        if result.is_none() {
            self.draw()?;
        }
        Ok(result)
    }

    fn draw(&mut self) -> io::Result<()> {
        let (cols, rows) = terminal::size()?;
        self.dashboard.page = usize::from(rows.saturating_sub(3)).max(1);
        self.dashboard.scroll_up(0);
        let mut bytes = Vec::new();
        self.dashboard
            .draw(&mut bytes, cols, rows, self.iteration, self.total)?;
        let update = self.frame.update(&bytes, cols, rows);
        let mut out = io::stderr().lock();
        out.write_all(&update)?;
        out.flush()
    }
}

impl Drop for LoopConsole {
    fn drop(&mut self) {
        let _ = execute!(
            io::stderr(),
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste,
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(dashboard: &mut Dashboard, text: &str) {
        for c in text.chars() {
            assert_eq!(dashboard.event(key(KeyCode::Char(c))), None);
        }
    }

    #[test]
    fn scrolling_keeps_history_stable_until_returning_live() {
        let mut ui = Dashboard {
            page: 4,
            ..Dashboard::default()
        };
        for n in 0..20 {
            ui.push(&format!("line {n}"));
        }
        ui.event(key(KeyCode::PageUp));
        assert_eq!(ui.scroll, 4);
        let end = ui.lines.len() - ui.scroll;
        ui.push("new output");
        assert_eq!(ui.lines.len() - ui.scroll, end);
        ui.event(key(KeyCode::Home));
        assert_eq!(ui.scroll, 17);
        ui.event(key(KeyCode::PageDown));
        assert_eq!(ui.scroll, 13);
        ui.event(key(KeyCode::End));
        assert_eq!(ui.scroll, 0);
        ui.push("live output");
        assert_eq!(ui.scroll, 0);
    }

    #[test]
    fn mouse_wheel_scrolls_while_composing_without_losing_the_draft() {
        let mut ui = Dashboard {
            page: 4,
            mode: Mode::Queue,
            draft: "draft".into(),
            ..Dashboard::default()
        };
        for n in 0..20 {
            ui.push(&format!("line {n}"));
        }
        for (kind, expected) in [
            (MouseEventKind::ScrollUp, 3),
            (MouseEventKind::ScrollDown, 0),
        ] {
            ui.event(Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }));
            assert_eq!(ui.scroll, expected);
            assert_eq!(ui.draft, "draft");
            assert_eq!(ui.mode, Mode::Queue);
        }
    }

    #[test]
    fn frames_update_only_changed_cells_without_clearing_screen() {
        let mut ui = Dashboard::default();
        let mut frame = Frame::default();
        let mut terminal = vt100::Parser::new(8, 100, 0);
        for text in ["long progress message", "short"] {
            ui.lines.clear();
            ui.push(text);
            let mut bytes = Vec::new();
            ui.draw(&mut bytes, 100, 8, 1, 2).unwrap();
            let update = frame.update(&bytes, 100, 8);
            if text == "short" {
                assert!(!update.windows(4).any(|s| s == b"\x1b[2J"));
                assert!(!update.windows(4).any(|s| s == b"\x1b[1J"));
            }
            terminal.process(&update);
            let rows: Vec<_> = terminal.screen().rows(0, 100).collect();
            assert_eq!(rows[1].trim(), text);
            assert!(rows[7].contains("Queue message"));
            let repeat = frame.update(&bytes, 100, 8);
            assert!(repeat.len() < 20, "unchanged frame: {repeat:?}");
        }
    }

    #[test]
    fn scrolled_render_shows_history_and_resize_preserves_footer() {
        let mut ui = Dashboard {
            page: 3,
            ..Dashboard::default()
        };
        for n in 0..20 {
            ui.push(&format!("line {n}"));
        }
        ui.event(key(KeyCode::PageUp));
        let mut frame = Frame::default();
        for (cols, rows) in [(100, 6), (60, 8)] {
            let mut bytes = Vec::new();
            ui.draw(&mut bytes, cols, rows, 1, 2).unwrap();
            let mut terminal = vt100::Parser::new(rows, cols, 0);
            terminal.process(&frame.update(&bytes, cols, rows));
            let lines: Vec<_> = terminal.screen().rows(0, cols).collect();
            assert!(lines.iter().any(|s| s.trim() == "line 16"));
            assert!(!lines.iter().any(|s| s.trim() == "line 19"));
            assert!(lines.last().unwrap().contains("Queue message"));
        }
    }

    #[test]
    fn queue_submits_literal_unicode_and_shell_metacharacters() {
        let mut ui = Dashboard::default();
        ui.event(key(KeyCode::Char('q')));
        let text = "fix café 🦀 & $HOME %PATH% `whoami` $(exit)";
        type_text(&mut ui, text);
        assert_eq!(
            ui.event(key(KeyCode::Enter)),
            Some(Action::Queue(text.to_owned()))
        );
        assert!(ui.draft.is_empty());
        assert_eq!(ui.mode, Mode::Watching);
    }

    #[test]
    fn interrupt_requires_explicit_yes_and_preserves_draft_when_declined() {
        let mut ui = Dashboard::default();
        ui.event(key(KeyCode::Char('i')));
        type_text(&mut ui, "repair failing tests");
        assert_eq!(ui.event(key(KeyCode::Enter)), None);
        assert_eq!(ui.mode, Mode::Confirm);
        assert!(ui
            .footer()
            .contains("This will restart the loop. Are you sure?"));
        for decline in [KeyCode::Enter, KeyCode::Char('n'), KeyCode::Char('N')] {
            ui.mode = Mode::Confirm;
            assert_eq!(ui.event(key(decline)), None);
            assert_eq!(ui.mode, Mode::Interrupt);
            assert_eq!(ui.draft, "repair failing tests");
        }
        ui.event(key(KeyCode::Enter));
        assert_eq!(
            ui.event(key(KeyCode::Char('y'))),
            Some(Action::Restart("repair failing tests".to_owned()))
        );
    }

    #[test]
    fn escape_cancels_every_editor_and_confirmation_without_action() {
        for mode in [Mode::Queue, Mode::Interrupt, Mode::Confirm] {
            let mut ui = Dashboard {
                mode,
                draft: "unsaved".into(),
                ..Dashboard::default()
            };
            assert_eq!(ui.event(key(KeyCode::Esc)), None);
            assert_eq!(ui.mode, Mode::Watching);
            assert!(ui.draft.is_empty());
        }
    }

    #[test]
    fn blank_messages_cannot_be_queued_or_restart_the_child() {
        for mode in [Mode::Queue, Mode::Interrupt] {
            let mut ui = Dashboard {
                mode,
                draft: " \n\t".into(),
                ..Dashboard::default()
            };
            assert_eq!(ui.event(key(KeyCode::Enter)), None);
            assert_eq!(ui.mode, mode);
        }
    }

    #[test]
    fn ctrl_c_stops_from_every_screen() {
        for mode in [Mode::Watching, Mode::Queue, Mode::Interrupt, Mode::Confirm] {
            let mut ui = Dashboard {
                mode,
                ..Dashboard::default()
            };
            let event = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert_eq!(ui.event(event), Some(Action::Quit));
        }
    }

    #[test]
    fn key_releases_do_not_duplicate_input_on_windows() {
        let mut ui = Dashboard {
            mode: Mode::Queue,
            ..Dashboard::default()
        };
        let mut event = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        ui.event(Event::Key(event));
        event.kind = KeyEventKind::Release;
        ui.event(Event::Key(event));
        assert_eq!(ui.draft, "a");
    }

    #[test]
    fn paste_cannot_trigger_hotkeys_or_confirm_a_restart() {
        for mode in [Mode::Watching, Mode::Confirm] {
            let mut ui = Dashboard {
                mode,
                ..Dashboard::default()
            };
            assert_eq!(ui.event(Event::Paste("yqi\r\n".into())), None);
            assert_eq!(ui.mode, mode);
        }
        let mut ui = Dashboard {
            mode: Mode::Interrupt,
            ..Dashboard::default()
        };
        ui.event(Event::Paste("first\r\nsecond\x03\x1b".into()));
        assert_eq!(ui.draft, "first\nsecond");
        assert_eq!(ui.mode, Mode::Interrupt);
    }

    #[test]
    fn backspace_removes_unicode_without_corrupting_utf8() {
        let mut ui = Dashboard {
            mode: Mode::Queue,
            ..Dashboard::default()
        };
        type_text(&mut ui, "a🦀é");
        ui.event(key(KeyCode::Backspace));
        assert_eq!(ui.draft, "a🦀");
        ui.event(key(KeyCode::Backspace));
        assert_eq!(ui.draft, "a");
    }

    #[test]
    fn output_is_bounded_while_composing_and_does_not_modify_draft() {
        let mut ui = Dashboard {
            mode: Mode::Queue,
            draft: "in progress".into(),
            ..Dashboard::default()
        };
        for n in 0..10_100 {
            ui.push(&format!("event {n}"));
        }
        assert_eq!(ui.lines.len(), 10_000);
        assert_eq!(ui.lines.back().unwrap(), "event 10099");
        assert_eq!(ui.draft, "in progress");
    }

    #[test]
    fn footer_stays_on_last_row_after_output_and_resize() {
        let mut ui = Dashboard::default();
        for _ in 0..30 {
            ui.push("progress line");
        }
        for (cols, rows) in [(120, 24), (80, 10), (40, 5), (12, 2), (1, 1)] {
            let mut bytes = Vec::new();
            ui.draw(&mut bytes, cols, rows, 2, 10).unwrap();
            let mut terminal = vt100::Parser::new(rows, cols, 0);
            terminal.process(&bytes);
            let footer = terminal.screen().rows(0, cols).last().unwrap();
            assert!(
                footer.starts_with("[q]") || cols < 4,
                "{cols}x{rows}: {footer}"
            );
            if cols == 120 {
                assert!(footer.contains("Queue message"));
                assert!(footer.contains("Interrupt with message"));
            }
        }
    }

    #[test]
    fn wide_text_never_wraps_over_the_footer_and_control_bytes_are_not_executed() {
        let mut ui = Dashboard::default();
        ui.push(&format!("{}\x1b[2J\x1b]0;hijack\x07", "界🦀é".repeat(80)));
        let mut bytes = Vec::new();
        ui.draw(&mut bytes, 80, 5, 1, 2).unwrap();
        assert!(!bytes.windows(4).any(|s| s == b"\x1b]0;"));
        let mut terminal = vt100::Parser::new(5, 80, 0);
        terminal.process(&bytes);
        let rows: Vec<_> = terminal.screen().rows(0, 80).collect();
        assert!(rows[0].contains("iteration 1/2"));
        assert!(rows[4].contains("Queue message"));
    }

    #[test]
    fn queued_text_round_trips_to_next_iteration_without_shell_interpretation() {
        let root = tempfile::tempdir().unwrap();
        let mut ui = Dashboard::default();
        ui.event(key(KeyCode::Char('q')));
        let text = "check C:\\Program Files\\app & $(exit) %PATH% café\nsecond line";
        ui.event(Event::Paste(text.into()));
        let Some(Action::Queue(message)) = ui.event(key(KeyCode::Enter)) else {
            panic!("missing queue action")
        };
        crate::loop_artifacts::queue_message(root.path(), &message).unwrap();
        assert_eq!(
            crate::loop_artifacts::drain_inbox(root.path(), 2).unwrap(),
            Some(1)
        );
        let delivered =
            std::fs::read_to_string(crate::loop_artifacts::next_message_path(root.path())).unwrap();
        assert!(delivered.contains(text));
        assert_eq!(
            crate::loop_artifacts::drain_inbox(root.path(), 3).unwrap(),
            None
        );
    }

    #[test]
    fn failed_interrupt_save_keeps_the_action_and_requires_confirmation_on_retry() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".clud"), "obstruct inbox").unwrap();
        let mut ui = Dashboard::default();
        assert!(!ui.save(root.path(), "important draft", Mode::Interrupt));
        assert_eq!(ui.draft, "important draft");
        assert_eq!(ui.mode, Mode::Interrupt);
        assert!(ui.footer().contains("Could not queue message"));
        assert_eq!(ui.event(key(KeyCode::Enter)), None);
        assert_eq!(ui.mode, Mode::Confirm);
        assert!(ui.footer().contains("Are you sure?"));
    }

    #[test]
    fn modified_shortcuts_and_unrelated_confirmation_keys_do_nothing() {
        let mut ui = Dashboard {
            mode: Mode::Confirm,
            draft: "draft".into(),
            ..Dashboard::default()
        };
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            assert_eq!(
                ui.event(Event::Key(KeyEvent::new(KeyCode::Char('y'), modifiers))),
                None
            );
        }
        assert_eq!(ui.event(key(KeyCode::Char('q'))), None);
        assert_eq!(ui.mode, Mode::Confirm);
        assert_eq!(ui.draft, "draft");
    }
}

//! The inline terminal selector every interactive picker renders through
//! (#1195): the launch-setup scope prompt, the bare-launch harness picker, and
//! `clud settings`.
//!
//! A selector supplies only what it shows ([`View`]) and how it reacts
//! ([`Selector::on_key`], [`Selector::on_tick`]). This module owns every
//! terminal concern, so a rendering fix reaches all of them at once:
//!
//! - raw mode and cursor hide/show, restored on every exit path;
//! - draining keys already pending when the selector opens, so the Enter that
//!   submitted the `clud` command is not taken as a confirmation;
//! - one key decoder, which ignores key releases (crossterm reports them as
//!   separate events on Windows);
//! - CRLF-only output. Raw mode clears `OPOST` on POSIX, so a bare `\n` moves
//!   down without returning to column zero and the menu walks diagonally
//!   (#1063, #1195). Supplied text containing `\n` becomes separate CRLF lines,
//!   and other control characters are dropped so they cannot move the cursor;
//! - redraw and erase that move back over exactly the physical rows drawn,
//!   including rows that wrap at the terminal width.
//!
//! The frame stays in normal scrollback: there is no alternate screen.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;

const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
/// Columns between the longest label and an inline note.
const INLINE_NOTE_GAP: usize = 3;
/// Indent of hint and footer lines.
const TEXT_INDENT: &str = "  ";
/// Indent of a note drawn below its row, aligned past `> [x] `.
const NOTE_INDENT: &str = "      ";

/// A decoded key a selector can react to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Up arrow or `k`.
    Up,
    /// Down arrow or `j`.
    Down,
    Enter,
    Escape,
    Space,
    Char(char),
}

/// One unit of input: a key, or Ctrl-C/Ctrl-D, which always aborts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    Interrupt,
}

/// What a selector draws. Built fresh for every frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    pub title: String,
    /// Lines under the title, indented.
    pub hints: Vec<String>,
    /// A blank line between the hints and the rows.
    pub gap: bool,
    pub rows: Vec<Row>,
    /// Lines under the rows, indented.
    pub footer: Vec<String>,
}

/// One selectable line: `> [x] Label` plus an optional note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Draws the `>` cursor.
    pub current: bool,
    /// Usually [`check_marker`]'s `[x]`/`[ ]`, or a value like `[auto]`.
    pub marker: String,
    pub label: String,
    pub note: Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    None,
    /// On the same line, aligned past the longest label among inline-note rows.
    Inline(String),
    /// On its own line under the row.
    Below(String),
}

pub fn check_marker(checked: bool) -> &'static str {
    if checked {
        "[x]"
    } else {
        "[ ]"
    }
}

/// What a key or tick does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step<T> {
    /// Nothing visible changed.
    Stay,
    /// Draw the view again.
    Redraw,
    /// Close the selector with this outcome.
    Done(T),
}

/// What happens to the frame when the selector closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnExit {
    /// Leave it in scrollback and move to a fresh line.
    Keep,
    /// Clear it, leaving the cursor where the frame started.
    Erase,
}

pub trait Selector {
    type Outcome;

    /// The frame to draw; `elapsed` is the time since the selector opened.
    fn view(&self, elapsed: Duration) -> View;

    fn on_key(&mut self, key: Key) -> Step<Self::Outcome>;

    /// How often to call [`Selector::on_tick`] while no key arrives. `None`
    /// waits for keys only.
    fn tick_interval(&self) -> Option<Duration> {
        None
    }

    fn on_tick(&mut self, _elapsed: Duration) -> Step<Self::Outcome> {
        Step::Stay
    }

    fn on_exit(&self) -> OnExit {
        OnExit::Keep
    }
}

/// Rendered bytes plus the physical rows they occupy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    bytes: Vec<u8>,
    rows: usize,
    columns: usize,
}

impl Frame {
    fn new(columns: usize) -> Self {
        Self {
            bytes: Vec::new(),
            rows: 0,
            columns,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Physical terminal rows, counting wraps, that a redraw must move back.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Append `text` as one CRLF line per embedded `\n`. Other control
    /// characters are dropped: a stray `\r` or escape sequence would move the
    /// cursor and break the row arithmetic.
    fn line(&mut self, text: &str) {
        for piece in text.split('\n') {
            let visible: String = piece.chars().filter(|c| !c.is_control()).collect();
            self.rows += physical_rows(visible.chars().count(), self.columns);
            self.bytes.extend_from_slice(visible.as_bytes());
            self.bytes.extend_from_slice(b"\r\n");
        }
    }
}

/// Rows a line of `chars` characters occupies at `columns` wide; `0` columns
/// means the width is unknown and each line counts once.
fn physical_rows(chars: usize, columns: usize) -> usize {
    if columns == 0 || chars == 0 {
        1
    } else {
        chars.div_ceil(columns)
    }
}

/// Lay out `view` for a terminal `columns` wide, drawing every option row.
pub fn render(view: &View, columns: usize) -> Frame {
    render_within(view, columns, 0)
}

/// Lay out `view` for a terminal `columns` wide and `rows` tall. `0` rows
/// means the height is unknown, and every option row is drawn.
///
/// A frame taller than the viewport cannot be redrawn: its top rows scroll into
/// scrollback, where cursor-up cannot reach, so each redraw would stack a new
/// copy (#1198). The title, hints and footer always show. When the option rows
/// do not fit, they become a window that keeps the current row visible, with a
/// line saying how many rows are hidden above and below. The frame stays within
/// `rows - 1` physical rows, so its last line feed never scrolls the top away.
pub fn render_within(view: &View, columns: usize, rows: usize) -> Frame {
    let inline_width = view
        .rows
        .iter()
        .filter(|row| matches!(row.note, Note::Inline(_)))
        .map(|row| row.label.chars().count())
        .max()
        .unwrap_or(0)
        + INLINE_NOTE_GAP;
    let row_lines: Vec<Vec<String>> = view
        .rows
        .iter()
        .map(|row| lines_for_row(row, inline_width))
        .collect();

    let mut head: Vec<String> = vec![view.title.clone()];
    head.extend(view.hints.iter().map(|hint| format!("{TEXT_INDENT}{hint}")));
    if view.gap {
        head.push(String::new());
    }
    let tail: Vec<String> = view
        .footer
        .iter()
        .map(|line| format!("{TEXT_INDENT}{line}"))
        .collect();

    let window = if rows == 0 {
        0..row_lines.len()
    } else {
        let fixed = lines_rows(&head, columns) + lines_rows(&tail, columns);
        let budget = rows.saturating_sub(1).saturating_sub(fixed);
        let heights: Vec<usize> = row_lines
            .iter()
            .map(|lines| lines_rows(lines, columns))
            .collect();
        let current = view.rows.iter().position(|row| row.current).unwrap_or(0);
        visible_rows(&heights, current, budget)
    };

    let mut frame = Frame::new(columns);
    for line in &head {
        frame.line(line);
    }
    if window.start > 0 {
        frame.line(&format!("{TEXT_INDENT}... {} more above", window.start));
    }
    for lines in &row_lines[window.clone()] {
        for line in lines {
            frame.line(line);
        }
    }
    if window.end < row_lines.len() {
        frame.line(&format!(
            "{TEXT_INDENT}... {} more below",
            row_lines.len() - window.end
        ));
    }
    for line in &tail {
        frame.line(line);
    }
    frame
}

/// The terminal lines one option row draws.
fn lines_for_row(row: &Row, inline_width: usize) -> Vec<String> {
    let lead = format!(
        "{} {} {}",
        if row.current { ">" } else { " " },
        row.marker,
        row.label
    );
    match &row.note {
        Note::None => vec![lead],
        Note::Inline(note) => {
            let pad = inline_width.saturating_sub(row.label.chars().count());
            vec![format!("{lead}{}{note}", " ".repeat(pad))]
        }
        Note::Below(note) => vec![lead, format!("{NOTE_INDENT}{note}")],
    }
}

/// Physical rows `lines` occupy, counted exactly as [`Frame::line`] draws them.
fn lines_rows(lines: &[String], columns: usize) -> usize {
    let mut frame = Frame::new(columns);
    for line in lines {
        frame.line(line);
    }
    frame.rows()
}

/// The option rows to draw: every row when they fit `budget`, otherwise a
/// contiguous range around `current`, grown alternately downward and upward.
/// Two rows of the budget are reserved for the "more above" and "more below"
/// lines once windowing starts, so the frame fits whichever side is hidden.
fn visible_rows(heights: &[usize], current: usize, budget: usize) -> std::ops::Range<usize> {
    if heights.is_empty() || heights.iter().sum::<usize>() <= budget {
        return 0..heights.len();
    }
    let available = budget.saturating_sub(2);
    let current = current.min(heights.len() - 1);
    let (mut start, mut end) = (current, current + 1);
    let mut used = heights[current];
    loop {
        let mut grew = false;
        if end < heights.len() && used + heights[end] <= available {
            used += heights[end];
            end += 1;
            grew = true;
        }
        if start > 0 && used + heights[start - 1] <= available {
            start -= 1;
            used += heights[start];
            grew = true;
        }
        if !grew {
            return start..end;
        }
    }
}

/// Where a selector's input, clock and width come from. [`run`] uses the real
/// terminal; tests script it.
pub trait Terminal {
    fn drain_pending(&mut self) -> io::Result<()>;
    /// The next decoded input, or `None` if `timeout` passes first.
    fn next_input(&mut self, timeout: Option<Duration>) -> io::Result<Option<Input>>;
    fn elapsed(&self) -> Duration;
    /// Terminal width in columns; `0` when unknown.
    fn columns(&self) -> usize;
    /// Terminal height in rows; `0` when unknown, which draws every option row.
    fn rows(&self) -> usize {
        0
    }
}

/// Run `selector` on the real terminal, writing its frames to `out`.
///
/// Ctrl-C and Ctrl-D return an [`io::ErrorKind::Interrupted`] error after the
/// frame is closed the same way a normal exit closes it.
pub fn run<S: Selector, W: Write>(out: &mut W, selector: &mut S) -> io::Result<S::Outcome> {
    let _raw = RawModeGuard::enable()?;
    // Raw mode changes input only. A legacy Windows console also needs VT
    // output processing, or every escape sequence below prints as literal text.
    // `main` already enabled it; re-assert it in case a child sharing the
    // console cleared it. Never crossterm's `supports_ansi`: its `Once`-latched
    // enable is what hid clud's missing startup enable (#1374).
    crate::console_setup::enable_console_vt_output();
    out.write_all(HIDE_CURSOR)?;
    out.flush()?;
    let mut terminal = CrosstermTerminal {
        started: Instant::now(),
    };
    let result = drive(out, selector, &mut terminal);
    let restored = out.write_all(SHOW_CURSOR).and_then(|()| out.flush());
    match result {
        Ok(outcome) => restored.map(|()| outcome),
        Err(error) => Err(error),
    }
}

/// The selector loop, independent of the real terminal.
pub fn drive<S, W, T>(out: &mut W, selector: &mut S, terminal: &mut T) -> io::Result<S::Outcome>
where
    S: Selector,
    W: Write,
    T: Terminal,
{
    terminal.drain_pending()?;
    let mut drawn = draw(
        out,
        &selector.view(terminal.elapsed()),
        terminal.columns(),
        terminal.rows(),
    )?;
    loop {
        let step = match terminal.next_input(selector.tick_interval())? {
            Some(Input::Interrupt) => {
                close(out, selector.on_exit(), drawn)?;
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "selection cancelled",
                ));
            }
            Some(Input::Key(key)) => selector.on_key(key),
            None => selector.on_tick(terminal.elapsed()),
        };
        match step {
            Step::Stay => {}
            Step::Redraw => {
                erase(out, drawn)?;
                drawn = draw(
                    out,
                    &selector.view(terminal.elapsed()),
                    terminal.columns(),
                    terminal.rows(),
                )?;
            }
            Step::Done(outcome) => {
                close(out, selector.on_exit(), drawn)?;
                return Ok(outcome);
            }
        }
    }
}

fn draw<W: Write>(out: &mut W, view: &View, columns: usize, rows: usize) -> io::Result<usize> {
    let frame = render_within(view, columns, rows);
    out.write_all(frame.bytes())?;
    out.flush()?;
    Ok(frame.rows())
}

fn erase<W: Write>(out: &mut W, rows: usize) -> io::Result<()> {
    // `\x1b[0A` moves up one row on many terminals, so zero rows writes nothing.
    if rows > 0 {
        write!(out, "\x1b[{rows}A\x1b[J")?;
    }
    Ok(())
}

fn close<W: Write>(out: &mut W, on_exit: OnExit, drawn: usize) -> io::Result<()> {
    match on_exit {
        OnExit::Keep => out.write_all(b"\r\n")?,
        OnExit::Erase => erase(out, drawn)?,
    }
    out.flush()
}

/// Decode a terminal event. Releases, non-key events and unmapped keys are
/// `None`.
pub fn decode(event: &Event) -> Option<Input> {
    let Event::Key(KeyEvent {
        code,
        modifiers,
        kind,
        ..
    }) = event
    else {
        return None;
    };
    if *kind == KeyEventKind::Release {
        return None;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        return matches!(code, KeyCode::Char('c' | 'd')).then_some(Input::Interrupt);
    }
    let key = match code {
        KeyCode::Up | KeyCode::Char('k') => Key::Up,
        KeyCode::Down | KeyCode::Char('j') => Key::Down,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Escape,
        KeyCode::Char(' ') => Key::Space,
        KeyCode::Char(character) => Key::Char(*character),
        _ => return None,
    };
    Some(Input::Key(key))
}

struct CrosstermTerminal {
    started: Instant,
}

impl Terminal for CrosstermTerminal {
    fn drain_pending(&mut self) -> io::Result<()> {
        drain_pending_events(|| event::poll(Duration::ZERO), event::read).map(drop)
    }

    fn next_input(&mut self, timeout: Option<Duration>) -> io::Result<Option<Input>> {
        loop {
            if let Some(timeout) = timeout {
                if !event::poll(timeout)? {
                    return Ok(None);
                }
            }
            if let Some(input) = decode(&event::read()?) {
                return Ok(Some(input));
            }
        }
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn columns(&self) -> usize {
        terminal::size().map_or(0, |(columns, _)| usize::from(columns))
    }

    fn rows(&self) -> usize {
        terminal::size().map_or(0, |(_, rows)| usize::from(rows))
    }
}

fn drain_pending_events<P, R>(mut poll: P, mut read: R) -> io::Result<usize>
where
    P: FnMut() -> io::Result<bool>,
    R: FnMut() -> io::Result<Event>,
{
    let mut drained = 0;
    while poll()? {
        let _ = read()?;
        drained += 1;
    }
    Ok(drained)
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// A scripted [`Terminal`] for selector tests across the crate.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::collections::VecDeque;

    /// `Some(input)` delivers a key; `None` lets a tick pass and advances the
    /// clock by `tick`. Running past the end of the script is an error, so a
    /// selector that fails to finish fails its test instead of hanging.
    pub(crate) struct ScriptedTerminal {
        script: VecDeque<Option<Input>>,
        now: Duration,
        tick: Duration,
        columns: usize,
        rows: usize,
        pub(crate) drained: bool,
    }

    impl ScriptedTerminal {
        pub(crate) fn new(
            script: impl IntoIterator<Item = Option<Input>>,
            tick: Duration,
            columns: usize,
        ) -> Self {
            Self {
                script: script.into_iter().collect(),
                now: Duration::ZERO,
                tick,
                columns,
                rows: 0,
                drained: false,
            }
        }

        /// Give the scripted terminal a height, so frames are windowed to it.
        pub(crate) fn with_rows(mut self, rows: usize) -> Self {
            self.rows = rows;
            self
        }
    }

    impl Terminal for ScriptedTerminal {
        fn drain_pending(&mut self) -> io::Result<()> {
            self.drained = true;
            Ok(())
        }

        fn next_input(&mut self, timeout: Option<Duration>) -> io::Result<Option<Input>> {
            match self.script.pop_front() {
                Some(Some(input)) => Ok(Some(input)),
                Some(None) => {
                    assert!(
                        timeout.is_some(),
                        "a tick was scripted for a selector that waits for keys only"
                    );
                    self.now += self.tick;
                    Ok(None)
                }
                None => Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "selector script exhausted",
                )),
            }
        }

        fn elapsed(&self) -> Duration {
            self.now
        }

        fn columns(&self) -> usize {
            self.columns
        }

        fn rows(&self) -> usize {
            self.rows
        }
    }

    pub(crate) fn key(key: Key) -> Option<Input> {
        Some(Input::Key(key))
    }

    /// Every line feed in `text` is part of a CRLF.
    pub(crate) fn assert_crlf_only(text: &str) {
        assert_eq!(
            text.matches('\n').count(),
            text.matches("\r\n").count(),
            "every line feed must be a CRLF while raw mode is on: {text:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{assert_crlf_only, key, ScriptedTerminal};
    use super::*;
    use crossterm::event::KeyEventState;

    struct Toggle {
        label: String,
        on: bool,
        exit: OnExit,
    }

    impl Selector for Toggle {
        type Outcome = bool;

        fn view(&self, _elapsed: Duration) -> View {
            View {
                title: "Toggle".to_string(),
                hints: vec!["Space flips".to_string()],
                gap: false,
                rows: vec![Row {
                    current: true,
                    marker: check_marker(self.on).to_string(),
                    label: self.label.clone(),
                    note: Note::None,
                }],
                footer: Vec::new(),
            }
        }

        fn on_key(&mut self, key: Key) -> Step<bool> {
            match key {
                Key::Space => {
                    self.on = !self.on;
                    Step::Redraw
                }
                Key::Enter => Step::Done(self.on),
                _ => Step::Stay,
            }
        }

        fn on_exit(&self) -> OnExit {
            self.exit
        }
    }

    fn toggle(label: &str, exit: OnExit) -> Toggle {
        Toggle {
            label: label.to_string(),
            on: false,
            exit,
        }
    }

    fn row(current: bool, label: &str, note: Note) -> Row {
        Row {
            current,
            marker: check_marker(current).to_string(),
            label: label.to_string(),
            note,
        }
    }

    fn tall_view(count: usize, current: usize) -> View {
        View {
            title: "Tall".to_string(),
            hints: vec!["hint".to_string()],
            gap: true,
            rows: (0..count)
                .map(|index| {
                    row(
                        index == current,
                        &format!("item-{index}"),
                        Note::Below(format!("note {index}")),
                    )
                })
                .collect(),
            footer: vec!["footer".to_string()],
        }
    }

    #[test]
    fn a_frame_that_fits_the_viewport_is_rendered_unchanged() {
        let view = tall_view(4, 1);
        assert_eq!(render_within(&view, 80, 100), render(&view, 80));
        assert_eq!(render_within(&view, 80, 0), render(&view, 80));
    }

    /// #1198: a frame taller than the terminal left its top rows in
    /// scrollback, where the cursor-up redraw cannot reach, so `clud settings`
    /// stacked a new copy on every keypress in a 30-row terminal.
    #[test]
    fn a_tall_frame_windows_its_rows_around_the_current_row_within_the_viewport() {
        for current in [0, 7, 20, 39] {
            let frame = render_within(&tall_view(40, current), 80, 12);
            let text = frame.text();
            assert!(
                frame.rows() <= 11,
                "current {current}: {} rows\n{text}",
                frame.rows()
            );
            assert!(
                text.contains(&format!("> [x] item-{current}\r\n")),
                "current {current}:\n{text}"
            );
            assert!(text.starts_with("Tall\r\n  hint\r\n\r\n"), "{text}");
            assert!(text.ends_with("  footer\r\n"), "{text}");
            assert_eq!(
                text.contains("more above"),
                current > 0,
                "current {current}:\n{text}"
            );
            assert_eq!(
                text.contains("more below"),
                current < 39,
                "current {current}:\n{text}"
            );
            assert_crlf_only(&text);
        }
    }

    #[test]
    fn wrapped_notes_count_toward_the_window() {
        let mut view = tall_view(10, 5);
        for row in &mut view.rows {
            row.note = Note::Below("x".repeat(45));
        }
        // At 20 columns each row is its label plus a note wrapping to 3 rows.
        let frame = render_within(&view, 20, 14);
        assert!(
            frame.rows() <= 13,
            "{} rows\n{}",
            frame.rows(),
            frame.text()
        );
        assert!(
            frame.text().contains("> [x] item-5\r\n"),
            "{}",
            frame.text()
        );
    }

    #[test]
    fn redraws_in_a_short_terminal_never_move_back_past_the_viewport() {
        struct List {
            current: usize,
        }

        impl Selector for List {
            type Outcome = usize;

            fn view(&self, _elapsed: Duration) -> View {
                tall_view(40, self.current)
            }

            fn on_key(&mut self, key: Key) -> Step<usize> {
                match key {
                    Key::Down => {
                        self.current = (self.current + 1).min(39);
                        Step::Redraw
                    }
                    Key::Enter => Step::Done(self.current),
                    _ => Step::Stay,
                }
            }
        }

        let script = std::iter::repeat_n(key(Key::Down), 25).chain([key(Key::Enter)]);
        let mut terminal = ScriptedTerminal::new(script, Duration::ZERO, 80).with_rows(12);
        let mut out = Vec::new();
        let chosen = drive(&mut out, &mut List { current: 0 }, &mut terminal).unwrap();
        assert_eq!(chosen, 25);

        let text = String::from_utf8(out).unwrap();
        let moves: Vec<usize> = text
            .split("\x1b[J")
            .filter_map(|chunk| {
                chunk
                    .rsplit_once("\x1b[")
                    .and_then(|(_, tail)| tail.strip_suffix('A'))
                    .and_then(|rows| rows.parse().ok())
            })
            .collect();
        assert_eq!(moves.len(), 25, "{text:?}");
        assert!(moves.iter().all(|rows| *rows <= 11), "{moves:?}");
        assert!(text.contains("> [x] item-25\r\n"), "{text:?}");
    }

    #[test]
    fn layout_indents_hints_aligns_inline_notes_and_places_notes_below() {
        let view = View {
            title: "Title".to_string(),
            hints: vec!["hint".to_string()],
            gap: true,
            rows: vec![
                row(true, "Short", Note::Inline("first".to_string())),
                row(false, "Much longer", Note::Inline("second".to_string())),
                row(false, "Plain", Note::None),
                row(false, "Noted", Note::Below("below".to_string())),
            ],
            footer: vec!["footer".to_string()],
        };
        assert_eq!(
            render(&view, 0).text(),
            "Title\r\n  hint\r\n\r\n> [x] Short         first\r\n  [ ] Much longer   second\r\n  [ ] Plain\r\n  [ ] Noted\r\n      below\r\n  footer\r\n"
        );
    }

    #[test]
    fn every_line_is_crlf_even_when_supplied_text_has_newlines_or_control_characters() {
        let view = View {
            title: "Two\nlines".to_string(),
            hints: vec!["carriage\rreturn\x1b[2Jescape".to_string()],
            gap: false,
            rows: vec![row(true, "label", Note::Below("note\nsecond".to_string()))],
            footer: vec!["tab\there".to_string()],
        };
        let frame = render(&view, 0);
        let text = frame.text();
        assert_crlf_only(&text);
        assert!(!text.contains('\x1b') && !text.contains('\t'), "{text:?}");
        assert!(!text.replace("\r\n", "").contains('\r'), "{text:?}");
        assert_eq!(frame.rows(), text.matches("\r\n").count());
    }

    #[test]
    fn physical_rows_count_lines_that_wrap_at_the_terminal_width() {
        assert_eq!(physical_rows(0, 10), 1);
        assert_eq!(physical_rows(10, 10), 1);
        assert_eq!(physical_rows(11, 10), 2);
        assert_eq!(physical_rows(25, 10), 3);
        assert_eq!(physical_rows(500, 0), 1, "unknown width counts one row");

        let view = View {
            title: "x".repeat(25),
            ..View::default()
        };
        assert_eq!(render(&view, 10).rows(), 3);
    }

    #[test]
    fn redraw_moves_back_over_exactly_the_rows_drawn_including_wraps() {
        // At 10 columns: title "Toggle" (1) + hint "  Space flips", 13
        // characters (2) + a 30-character row (3) = 6 rows.
        let mut selector = toggle(&"y".repeat(24), OnExit::Keep);
        let mut terminal = ScriptedTerminal::new(
            [key(Key::Space), key(Key::Enter)],
            Duration::from_millis(100),
            10,
        );
        let mut out = Vec::new();
        assert!(drive(&mut out, &mut selector, &mut terminal).unwrap());
        let text = String::from_utf8(out).unwrap();
        assert!(terminal.drained, "pending input must be drained first");
        assert_eq!(text.matches("\x1b[6A\x1b[J").count(), 1, "{text:?}");
        assert_crlf_only(&text);
    }

    #[test]
    fn keep_leaves_the_frame_and_erase_clears_it() {
        let mut out = Vec::new();
        let mut terminal = ScriptedTerminal::new([key(Key::Enter)], Duration::ZERO, 80);
        drive(&mut out, &mut toggle("a", OnExit::Keep), &mut terminal).unwrap();
        let kept = String::from_utf8(out).unwrap();
        assert!(
            kept.ends_with("\r\n\r\n") && !kept.contains("\x1b["),
            "{kept:?}"
        );

        let mut out = Vec::new();
        let mut terminal = ScriptedTerminal::new([key(Key::Enter)], Duration::ZERO, 80);
        drive(&mut out, &mut toggle("a", OnExit::Erase), &mut terminal).unwrap();
        let erased = String::from_utf8(out).unwrap();
        assert!(erased.ends_with("\x1b[3A\x1b[J"), "{erased:?}");
    }

    #[test]
    fn interrupt_closes_the_frame_and_returns_interrupted() {
        let mut out = Vec::new();
        let mut terminal = ScriptedTerminal::new(
            [key(Key::Char('x')), Some(Input::Interrupt)],
            Duration::ZERO,
            80,
        );
        let error = drive(&mut out, &mut toggle("a", OnExit::Erase), &mut terminal).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(String::from_utf8(out).unwrap().ends_with("\x1b[3A\x1b[J"));
    }

    fn key_event(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn decoder_maps_navigation_and_interrupts_and_ignores_releases() {
        let press = |code| decode(&key_event(code, KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(press(KeyCode::Up), key(Key::Up));
        assert_eq!(press(KeyCode::Char('k')), key(Key::Up));
        assert_eq!(press(KeyCode::Down), key(Key::Down));
        assert_eq!(press(KeyCode::Char('j')), key(Key::Down));
        assert_eq!(press(KeyCode::Enter), key(Key::Enter));
        assert_eq!(press(KeyCode::Esc), key(Key::Escape));
        assert_eq!(press(KeyCode::Char(' ')), key(Key::Space));
        assert_eq!(press(KeyCode::Char('q')), key(Key::Char('q')));
        assert_eq!(press(KeyCode::F(5)), None);

        for character in ['c', 'd'] {
            assert_eq!(
                decode(&key_event(
                    KeyCode::Char(character),
                    KeyModifiers::CONTROL,
                    KeyEventKind::Press
                )),
                Some(Input::Interrupt)
            );
        }
        assert_eq!(
            decode(&key_event(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press
            )),
            None
        );
        assert_eq!(
            decode(&key_event(
                KeyCode::Down,
                KeyModifiers::NONE,
                KeyEventKind::Release
            )),
            None,
            "a release must not repeat the press"
        );
        assert_eq!(
            decode(&key_event(
                KeyCode::Down,
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            )),
            key(Key::Down)
        );
        assert_eq!(decode(&Event::Resize(80, 24)), None);
    }

    #[test]
    fn pending_input_is_drained_before_the_selector_accepts_input() {
        use std::cell::RefCell;
        use std::collections::VecDeque;

        let events = RefCell::new(VecDeque::from([
            key_event(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Press),
            key_event(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press),
        ]));
        let drained = drain_pending_events(
            || Ok(!events.borrow().is_empty()),
            || Ok(events.borrow_mut().pop_front().unwrap()),
        )
        .unwrap();
        assert_eq!(drained, 2);
        assert!(events.borrow().is_empty());
    }

    /// #1195: every inline selector renders through this module. A selector
    /// that drives the terminal itself brings back its own line endings,
    /// redraw arithmetic and key loop, which is how the harness picker and
    /// `clud settings` kept the bare-LF bug after #1106 fixed one copy.
    #[test]
    fn migrated_selectors_never_drive_the_terminal_themselves() {
        for (name, source) in [
            ("harness_picker.rs", include_str!("harness_picker.rs")),
            ("launch_setup.rs", include_str!("launch_setup.rs")),
            ("loop_restart.rs", include_str!("loop_restart.rs")),
            (
                "foreground_runtime.rs",
                include_str!("foreground_runtime.rs"),
            ),
            ("settings_tui.rs", include_str!("settings_tui.rs")),
            (
                "session_history/picker.rs",
                include_str!("session_history/picker.rs"),
            ),
            (
                "self_install/picker.rs",
                include_str!("self_install/picker.rs"),
            ),
        ] {
            // Only production code: tests legitimately assert on the escape
            // sequences the shared renderer emits. Colored output elsewhere in
            // a module is fine; what is forbidden is driving a selector frame:
            // raw mode, event reads, per-line writes, cursor visibility, and
            // the clear-to-end that follows a cursor-up redraw.
            let source = source.split("#[cfg(test)]").next().unwrap_or(source);
            for forbidden in [
                "enable_raw_mode",
                "disable_raw_mode",
                "event::read",
                "event::poll",
                "writeln!(out",
                "?25l",
                "?25h",
                "\\x1b[J",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "{name} contains `{forbidden}`; render through crate::selector instead"
                );
            }
        }
    }
}

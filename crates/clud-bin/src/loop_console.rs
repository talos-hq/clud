//! Interactive dashboard for headless loop children. Never runs beside a child TUI.
use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::{cursor, execute, queue, style, terminal};
use unicode_width::UnicodeWidthChar;

const MAX_LOGICAL_LINES: usize = 10_000;
/// Bounds one tick so a mouse-motion flood cannot starve the child's output.
const MAX_EVENTS_PER_TICK: usize = 4096;
/// Output can arrive far faster than a terminal can usefully repaint.
const MIN_DRAW_INTERVAL: Duration = Duration::from_millis(33);
/// Later chunks of one paste can land on the next ticks.
const PASTE_CONTINUATION: Duration = Duration::from_millis(50);
/// Text keys in one tick that no typist produces. Windows consoles have no
/// bracketed paste, so crossterm delivers a paste as individual key events.
const PASTE_BURST_CHARS: usize = 2;

static DASHBOARD_ACTIVE: AtomicBool = AtomicBool::new(false);

/// True while a dashboard owns the alternate screen. Background diagnostics
/// must not `eprintln!` then: the bytes land inside the frame, and the
/// cell-diff renderer never repaints cells it believes are unchanged.
fn owns_stderr() -> bool {
    DASHBOARD_ACTIVE.load(Ordering::SeqCst)
}

const MAX_DEFERRED_DIAGNOSTICS: usize = 1000;
static DEFERRED_DIAGNOSTICS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static DROPPED_DIAGNOSTICS: AtomicUsize = AtomicUsize::new(0);

/// Print a diagnostic now, or queue it for the chat stream if a dashboard owns the screen.
pub(crate) fn eprint_diagnostic(line: &str) {
    if !owns_stderr() {
        eprintln!("{line}");
        return;
    }
    match DEFERRED_DIAGNOSTICS.lock() {
        Ok(mut deferred) if deferred.len() < MAX_DEFERRED_DIAGNOSTICS => {
            deferred.push(line.to_owned());
        }
        _ => {
            DROPPED_DIAGNOSTICS.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// `eprintln!` for code that can run while a dashboard owns the screen.
macro_rules! diagnostic {
    ($($arg:tt)*) => {
        $crate::loop_console::eprint_diagnostic(&std::fmt::format(format_args!($($arg)*)))
    };
}
pub(crate) use diagnostic;

fn take_deferred_diagnostics() -> Vec<String> {
    DEFERRED_DIAGNOSTICS
        .lock()
        .map(|mut deferred| std::mem::take(&mut *deferred))
        .unwrap_or_default()
}

fn flush_deferred_diagnostics() {
    for line in take_deferred_diagnostics() {
        eprintln!("{line}");
    }
    let dropped = DROPPED_DIAGNOSTICS.swap(0, Ordering::SeqCst);
    if dropped > 0 {
        eprintln!("[clud] {dropped} more diagnostic lines were dropped");
    }
}

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
    /// Display rows, wrapped to `width`; `scroll` counts these.
    lines: VecDeque<String>,
    /// Sanitized source lines with their display-row count, for rewrap and transcript.
    logical: VecDeque<(String, usize)>,
    width: usize,
    scroll: usize,
    page: usize,
}

/// Remove terminal control sequences from one child line, keeping its text.
/// Dropping only the control characters would leave `[31m` in the chat.
fn sanitize(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            '\u{9b}' => skip_csi(&mut chars),
            '\t' => out.push_str("    "),
            // A carriage return redraws the line: the text after it is what shows.
            '\r' if chars.peek().is_some() => out.clear(),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for c in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            break;
        }
    }
}

fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => skip_csi(chars),
        // OSC, DCS, SOS, PM and APC run to BEL or ST.
        Some(']' | 'P' | 'X' | '^' | '_') => {
            while let Some(c) = chars.next() {
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' {
                    chars.next();
                    break;
                }
            }
        }
        _ => {}
    }
}

/// Hard-wrap to `width` display cells. Zero means no wrapping is known yet.
fn wrap(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_owned()];
    }
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    for c in line.chars() {
        let cells = c.width().unwrap_or(0);
        if used + cells > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push(c);
        used += cells;
    }
    rows.push(row);
    rows
}

fn is_text_key(key: &KeyEvent) -> bool {
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    matches!(key.code, KeyCode::Enter | KeyCode::Tab) && plain
        || matches!(key.code, KeyCode::Char(c) if plain || is_altgr_text(*key, c))
}

/// AltGr reads as Ctrl+Alt on Windows; it types `@` or `€` on many layouts.
/// ASCII letters and digits stay shortcuts, so Ctrl+Alt+Y cannot confirm.
fn is_altgr_text(key: KeyEvent, c: char) -> bool {
    key.modifiers
        .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && !c.is_control()
        && !c.is_ascii_alphanumeric()
}

/// Turn text keys that arrived faster than typing into one paste, so pasted
/// text cannot trigger hotkeys or submit a draft where the terminal has no
/// bracketed paste. Returns the events and whether a paste was recognized.
fn coalesce_paste(events: Vec<Event>, continuing: bool) -> (Vec<Event>, bool) {
    let mut out = Vec::with_capacity(events.len());
    let mut run: Vec<Event> = Vec::new();
    let mut found = false;
    for event in events {
        match &event {
            Event::Key(key) if key.kind == KeyEventKind::Release => {}
            Event::Key(key) if is_text_key(key) => run.push(event),
            _ => {
                found |= flush_run(&mut run, &mut out, continuing);
                out.push(event);
            }
        }
    }
    found |= flush_run(&mut run, &mut out, continuing);
    (out, found)
}

fn flush_run(run: &mut Vec<Event>, out: &mut Vec<Event>, continuing: bool) -> bool {
    let keys = || {
        run.iter().filter_map(|event| match event {
            Event::Key(key) => Some(key.code),
            _ => None,
        })
    };
    let chars = keys().filter(|c| matches!(c, KeyCode::Char(_))).count();
    let submits = chars > 0 && keys().any(|c| c == KeyCode::Enter);
    if !(continuing || chars >= PASTE_BURST_CHARS || submits) || run.is_empty() {
        out.append(run);
        return false;
    }
    let text = keys()
        .filter_map(|code| match code {
            KeyCode::Char(c) => Some(c),
            KeyCode::Enter => Some('\n'),
            KeyCode::Tab => Some(' '),
            _ => None,
        })
        .collect();
    run.clear();
    out.push(Event::Paste(text));
    true
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
                // Terminals disagree on paste newlines: CRLF, LF or a bare CR.
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                self.draft
                    .extend(text.chars().filter(|c| !c.is_control() || *c == '\n'));
                None
            }
            _ => None,
        }
    }

    fn key(&mut self, key: KeyEvent) -> Option<Action> {
        let is_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if is_ctrl
            && !key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Some(Action::Quit);
        }
        if matches!(key.code, KeyCode::Char(c) if is_altgr_text(key, c)) {
            return self.mode_key(KeyEvent::new(key.code, KeyModifiers::NONE));
        }
        if is_ctrl || key.modifiers.contains(KeyModifiers::ALT) {
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
        for line in line.trim_end_matches(['\r', '\n']).split('\n') {
            let clean = sanitize(line);
            let rows = wrap(&clean, self.width);
            self.logical.push_back((clean, rows.len()));
            if self.scroll > 0 {
                self.scroll = self.scroll.saturating_add(rows.len());
            }
            self.lines.extend(rows);
        }
        while self.logical.len() > MAX_LOGICAL_LINES {
            if let Some((_, rows)) = self.logical.pop_front() {
                self.lines.drain(..rows.min(self.lines.len()));
            }
        }
        self.scroll_up(0);
    }

    fn set_width(&mut self, width: usize) {
        if width == self.width {
            return;
        }
        let (old_rows, old_scroll) = (self.lines.len(), self.scroll);
        self.width = width;
        self.lines.clear();
        for (line, count) in &mut self.logical {
            let rows = wrap(line, width);
            *count = rows.len();
            self.lines.extend(rows);
        }
        // Rewrapping changes the row count: keep the same share of history above live.
        self.scroll = old_scroll * self.lines.len() / old_rows.max(1);
        self.scroll_up(0);
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
        write_row(out, 0, cols, &header, true)?;
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
                false,
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
            write_row(out, rows - 2, cols, &editor, false)?;
        }
        write_row(out, rows.saturating_sub(1), cols, &self.footer(), true)?;
        out.flush()
    }
}

/// Terminals disagree on how these join neighbours into one glyph. Without
/// them each character's width is what `unicode-width` and terminals agree on.
fn renders_alone(c: char) -> bool {
    !c.is_control() && !matches!(c, '\u{200d}' | '\u{fe0e}' | '\u{fe0f}')
}

// Feeding only printable characters prevents child output and pastes from
// injecting VT commands. Stopping short of the last column prevents autowrap.
fn write_row(
    out: &mut impl Write,
    row: u16,
    cols: u16,
    text: &str,
    accent: bool,
) -> io::Result<()> {
    queue!(out, cursor::MoveTo(0, row))?;
    if accent {
        queue!(out, style::SetForegroundColor(style::Color::Cyan))?;
    }
    let limit = usize::from(cols.saturating_sub(2));
    let mut used = 0;
    for c in text.chars().filter(|c| renders_alone(*c)) {
        used += c.width().unwrap_or(0);
        if used > limit {
            break;
        }
        let mut buf = [0; 4];
        out.write_all(c.encode_utf8(&mut buf).as_bytes())?;
    }
    queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    if accent {
        queue!(out, style::ResetColor)?;
    }
    Ok(())
}

/// Split a drawn frame at each row's absolute `CSI <row>;1H`. Row text holds
/// no ESC, so the only other sequences are the colors inside a row.
fn split_rows(bytes: &[u8]) -> Vec<&[u8]> {
    let is_row_start = |at: usize| {
        let Some(rest) = bytes[at..].strip_prefix(b"\x1b[") else {
            return false;
        };
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        digits > 0 && rest[digits..].starts_with(b";1H")
    };
    let starts: Vec<usize> = (0..bytes.len()).filter(|at| is_row_start(*at)).collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, start)| &bytes[*start..starts.get(n + 1).copied().unwrap_or(bytes.len())])
        .collect()
}

#[derive(Default)]
struct Frame {
    size: (u16, u16),
    rows: Vec<Vec<u8>>,
}

impl Frame {
    /// The bytes that bring the terminal to this frame; empty when nothing
    /// changed. Whole rows at absolute positions, each erased to end of line,
    /// so a width disagreement with the terminal cannot outlive its row. It
    /// never clears the screen, which is what makes a terminal flash.
    fn update(&mut self, bytes: &[u8], cols: u16, rows: u16) -> Vec<u8> {
        // Below three rows, several drawn rows share one screen row, so the
        // last writer wins only if all of them are written.
        let resized = self.size != (cols, rows) || rows < 3;
        let next = split_rows(bytes);
        let mut body = Vec::new();
        if resized {
            // Some terminals show the cursor again after a resize.
            body.extend_from_slice(b"\x1b[?25l");
        }
        for (n, row) in next.iter().enumerate() {
            if resized || self.rows.get(n).map(Vec::as_slice) != Some(*row) {
                body.extend_from_slice(row);
            }
        }
        self.size = (cols, rows);
        self.rows = next.iter().map(|row| row.to_vec()).collect();
        if body.is_empty() {
            return body;
        }
        // Terminals that support it present the frame atomically; others ignore it.
        let mut output = b"\x1b[?2026h".to_vec();
        output.extend_from_slice(&body);
        output.extend_from_slice(b"\x1b[?2026l");
        output
    }
}

pub(crate) struct LoopConsole {
    root: PathBuf,
    dashboard: Dashboard,
    iteration: u32,
    total: u32,
    frame: Frame,
    dirty: bool,
    last_draw: Option<Instant>,
    paste_until: Option<Instant>,
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
            dirty: false,
            last_draw: None,
            paste_until: None,
        };
        DASHBOARD_ACTIVE.store(true, Ordering::SeqCst);
        // Construct the guard first so a partial setup failure still restores the terminal.
        execute!(
            io::stderr(),
            terminal::EnterAlternateScreen,
            terminal::DisableLineWrap,
            cursor::Hide,
            crossterm::event::EnableMouseCapture
        )?;
        // crossterm reads console key events on Windows and has no paste parser
        // there, so asking for bracketed paste could only type its markers.
        if !crate::platform::host::WINDOWS {
            execute!(io::stderr(), crossterm::event::EnableBracketedPaste)?;
        }
        console.draw()?;
        Ok(Some(console))
    }

    pub(crate) fn transcript(&self) -> String {
        self.dashboard
            .logical
            .iter()
            .map(|(line, _)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn set_iteration(&mut self, iteration: u32) {
        self.iteration = iteration;
        self.dirty = true;
    }

    /// Buffer one output line; the next `input` tick paints it.
    pub(crate) fn line(&mut self, line: &str) {
        self.dashboard.push(line);
        self.dirty = true;
    }

    /// One UI tick: apply every pending input event, then repaint if due.
    pub(crate) fn input(&mut self) -> io::Result<Option<Action>> {
        let mut events = Vec::new();
        while events.len() < MAX_EVENTS_PER_TICK && crossterm::event::poll(Duration::ZERO)? {
            events.push(crossterm::event::read()?);
        }
        for line in take_deferred_diagnostics() {
            self.line(&line);
        }
        let now = Instant::now();
        let continuing = self.paste_until.is_some_and(|until| now < until);
        let (events, pasted) = coalesce_paste(events, continuing);
        if pasted {
            self.paste_until = Some(now + PASTE_CONTINUATION);
        }
        for event in events {
            if let Some(action) = self.apply(event) {
                return Ok(Some(action));
            }
        }
        if self.dirty
            && self
                .last_draw
                .is_none_or(|last| now.duration_since(last) >= MIN_DRAW_INTERVAL)
        {
            self.draw()?;
        }
        Ok(None)
    }

    fn apply(&mut self, event: Event) -> Option<Action> {
        let moved = matches!(
            &event,
            Event::Mouse(mouse) if !matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            )
        );
        self.dirty |= !moved;
        match self.dashboard.event(event)? {
            Action::Queue(text) => {
                self.dashboard.save(&self.root, &text, Mode::Queue);
                None
            }
            Action::Restart(text) => self
                .dashboard
                .save(&self.root, &text, Mode::Interrupt)
                .then_some(Action::Restart(text)),
            Action::Quit => Some(Action::Quit),
        }
    }

    fn draw(&mut self) -> io::Result<()> {
        let (cols, rows) = terminal::size()?;
        self.dirty = false;
        self.last_draw = Some(Instant::now());
        self.dashboard.page = usize::from(rows.saturating_sub(3)).max(1);
        // `write_row` stops before the cell that would reach the last column.
        self.dashboard
            .set_width(usize::from(cols.saturating_sub(2)).max(1));
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
        DASHBOARD_ACTIVE.store(false, Ordering::SeqCst);
        if !crate::platform::host::WINDOWS {
            let _ = execute!(io::stderr(), crossterm::event::DisableBracketedPaste);
        }
        // One at a time: `execute!` stops at the first failure, and a failed
        // console-mode call must not leave the user on the alternate screen.
        let mut err = io::stderr();
        let _ = execute!(err, crossterm::event::DisableMouseCapture);
        let _ = execute!(err, cursor::Show);
        let _ = execute!(err, terminal::EnableLineWrap);
        let _ = execute!(err, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
        // The alternate screen is gone: this is the only copy left in scrollback.
        let transcript = self.transcript();
        if !transcript.is_empty() {
            eprintln!("{transcript}");
        }
        flush_deferred_diagnostics();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(text: &str) -> Vec<Event> {
        text.chars()
            .map(|c| match c {
                '\r' => key(KeyCode::Enter),
                c => key(KeyCode::Char(c)),
            })
            .collect()
    }

    #[test]
    fn key_burst_is_a_paste_so_it_cannot_activate_actions_without_bracketed_paste() {
        for (mode, text) in [
            (Mode::Watching, "qiy\r"),
            (Mode::Confirm, "yes\r"),
            (Mode::Confirm, "y\r"),
        ] {
            let mut ui = Dashboard {
                mode,
                draft: "keep".into(),
                ..Dashboard::default()
            };
            let (events, pasted) = coalesce_paste(press(text), false);
            assert!(pasted, "{text:?}");
            for event in events {
                assert_eq!(ui.event(event), None);
            }
            assert_eq!(ui.mode, mode);
        }
    }

    #[test]
    fn multiline_key_burst_stays_in_the_draft_instead_of_submitting() {
        let (events, _) = coalesce_paste(press("first\rsecond\r"), false);
        let mut ui = Dashboard {
            mode: Mode::Queue,
            ..Dashboard::default()
        };
        for event in events {
            assert_eq!(ui.event(event), None);
        }
        assert_eq!(ui.draft, "first\nsecond\n");
        assert_eq!(ui.mode, Mode::Queue);
    }

    #[test]
    fn typing_is_not_a_paste_and_interrupting_keys_keep_their_order() {
        for text in ["q", "\r"] {
            let (events, pasted) = coalesce_paste(press(text), false);
            assert!(!pasted);
            assert_eq!(events, press(text));
        }
        let mut events = press("abcdef");
        events.push(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        let (events, pasted) = coalesce_paste(events, false);
        assert!(pasted);
        assert!(matches!(events[0], Event::Paste(_)));
        assert_eq!(
            Dashboard::default().event(events[1].clone()),
            Some(Action::Quit)
        );
    }

    #[test]
    fn paste_continuation_and_release_events_do_not_leak_keys() {
        let mut burst = press("y");
        let mut release = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        burst.push(Event::Key(release));
        let (events, pasted) = coalesce_paste(burst, true);
        assert!(pasted);
        assert_eq!(events, vec![Event::Paste("y".into())]);
    }

    #[test]
    fn altgr_text_is_typed_but_ctrl_alt_letters_stay_inert() {
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let mut ui = Dashboard {
            mode: Mode::Queue,
            ..Dashboard::default()
        };
        for c in ['@', '€', '{'] {
            assert_eq!(
                ui.event(Event::Key(KeyEvent::new(KeyCode::Char(c), altgr))),
                None
            );
        }
        assert_eq!(ui.draft, "@€{");
        ui.mode = Mode::Confirm;
        for c in ['y', 'c'] {
            assert_eq!(
                ui.event(Event::Key(KeyEvent::new(KeyCode::Char(c), altgr))),
                None
            );
        }
        assert_eq!(ui.mode, Mode::Confirm);
    }

    #[test]
    fn caps_lock_ctrl_c_still_stops() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL));
        assert_eq!(Dashboard::default().event(event), Some(Action::Quit));
    }

    #[test]
    fn sanitize_keeps_text_and_removes_every_sequence_family() {
        for (raw, clean) in [
            ("\x1b[31mred\x1b[0m", "red"),
            ("\x1b]0;title\x07after", "after"),
            ("\x1b]8;;http://x\x1b\\link", "link"),
            ("\x1bPdata\x1b\\x", "x"),
            ("\u{9b}2Jx", "x"),
            ("a\tb", "a    b"),
            ("10%\r20%", "20%"),
            ("done\r", "done"),
            ("truncated\x1b[3", "truncated"),
        ] {
            assert_eq!(sanitize(raw), clean, "{raw:?}");
        }
    }

    #[test]
    fn long_lines_wrap_and_rewrap_on_resize_without_losing_text() {
        let mut ui = Dashboard {
            width: 10,
            page: 4,
            ..Dashboard::default()
        };
        ui.push(&"x".repeat(25));
        assert_eq!(ui.lines.len(), 3);
        ui.set_width(5);
        assert_eq!(ui.lines.len(), 5);
        assert_eq!(ui.logical[0].0.len(), 25);
        ui.push("");
        assert_eq!(ui.lines.back().unwrap(), "");
    }

    #[test]
    fn wide_characters_wrap_on_cell_widths() {
        assert_eq!(wrap("界界界", 4), vec!["界界", "界"]);
        assert_eq!(wrap("界", 1), vec!["界"]);
    }

    #[test]
    fn scroll_position_is_stable_when_wrapped_output_arrives_and_history_evicts() {
        let mut ui = Dashboard {
            width: 10,
            page: 2,
            ..Dashboard::default()
        };
        for _ in 0..MAX_LOGICAL_LINES {
            ui.push(&"y".repeat(15));
        }
        ui.scroll_up(5);
        let scroll = ui.scroll;
        ui.push(&"z".repeat(15));
        assert_eq!(ui.scroll, scroll + 2);
        assert_eq!(ui.logical.len(), MAX_LOGICAL_LINES);
        assert_eq!(ui.lines.len(), MAX_LOGICAL_LINES * 2);
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(dashboard: &mut Dashboard, text: &str) {
        for c in text.chars() {
            assert_eq!(dashboard.event(key(KeyCode::Char(c))), None);
        }
    }

    #[test]
    fn no_frame_ever_clears_the_screen_and_an_unchanged_frame_writes_nothing() {
        let mut ui = Dashboard::default();
        let mut frame = Frame::default();
        ui.push("hello");
        for (cols, rows) in [(80, 10), (80, 10), (60, 8), (100, 20)] {
            let mut bytes = Vec::new();
            ui.draw(&mut bytes, cols, rows, 1, 2).unwrap();
            let update = frame.update(&bytes, cols, rows);
            for clear in [&b"\x1b[2J"[..], b"\x1b[J", b"\x1b[0J", b"\x1b[1J"] {
                assert!(!update.windows(clear.len()).any(|s| s == clear));
            }
            assert!(frame.update(&bytes, cols, rows).is_empty());
        }
    }

    #[test]
    fn resize_repaint_overwrites_stale_cells_without_clearing() {
        let mut ui = Dashboard::default();
        let mut frame = Frame::default();
        ui.push("first line of long text");
        let mut terminal = vt100::Parser::new(8, 80, 0);
        let mut bytes = Vec::new();
        ui.draw(&mut bytes, 80, 8, 1, 2).unwrap();
        terminal.process(&frame.update(&bytes, 80, 8));
        ui.lines.clear();
        ui.push("x");
        let mut bytes = Vec::new();
        ui.draw(&mut bytes, 70, 8, 1, 2).unwrap();
        terminal.process(&frame.update(&bytes, 70, 8));
        assert!(!terminal.screen().contents().contains("first line"));
    }

    #[test]
    fn a_changed_row_is_rewritten_whole_and_other_rows_are_untouched() {
        let mut ui = Dashboard::default();
        let mut frame = Frame::default();
        ui.push("⚠\u{fe0f} 👨\u{200d}💻 same");
        ui.push("old");
        let mut bytes = Vec::new();
        ui.draw(&mut bytes, 80, 10, 1, 2).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(['\u{fe0f}', '\u{200d}']));
        frame.update(&bytes, 80, 10);
        ui.lines.pop_back();
        ui.push("new");
        let mut bytes = Vec::new();
        ui.draw(&mut bytes, 80, 10, 1, 2).unwrap();
        let update = String::from_utf8(frame.update(&bytes, 80, 10)).unwrap();
        assert!(update.contains("\x1b[3;1Hnew"), "{update:?}");
        assert!(!update.contains("same"), "{update:?}");
    }

    #[test]
    fn resizing_while_scrolled_keeps_the_share_of_history_above_live() {
        let mut ui = Dashboard {
            width: 10,
            page: 2,
            ..Dashboard::default()
        };
        for _ in 0..100 {
            ui.push(&"x".repeat(10));
        }
        ui.scroll = 50;
        ui.set_width(5);
        assert_eq!(ui.scroll, 100);
    }

    #[test]
    fn deferred_diagnostics_wait_for_the_dashboard_to_close() {
        DASHBOARD_ACTIVE.store(true, Ordering::SeqCst);
        eprint_diagnostic("later");
        DASHBOARD_ACTIVE.store(false, Ordering::SeqCst);
        let held = std::mem::take(&mut *DEFERRED_DIAGNOSTICS.lock().unwrap());
        assert!(held.contains(&"later".to_owned()));
    }

    #[test]
    fn tiny_terminals_write_every_row_because_rows_share_a_screen_line() {
        let ui = Dashboard::default();
        let mut frame = Frame::default();
        let mut bytes = Vec::new();
        ui.draw(&mut bytes, 80, 2, 1, 2).unwrap();
        frame.update(&bytes, 80, 2);
        let mut next = Vec::new();
        ui.draw(&mut next, 80, 2, 2, 2).unwrap();
        let mut terminal = vt100::Parser::new(2, 80, 0);
        terminal.process(&frame.update(&next, 80, 2));
        assert!(!terminal.screen().contents().contains("iteration"));
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

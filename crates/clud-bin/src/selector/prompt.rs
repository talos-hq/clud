//! The prompt layout ([`Theme::Prompt`]): a calm, aligned screen for a
//! decision such as the native installer's.
//!
//! ```text
//!
//!   Install clud 2.8.65
//!
//!   Version    2.8.65
//!   Location   ~/.local/bin/clud
//!
//!   > Install
//!     Not now
//!
//!   Up/Down move, Enter select, Esc cancel
//! ```
//!
//! The current row carries a pointer glyph, so the choice never depends on
//! colour alone. Colour and the non-ASCII glyphs follow a [`Style`] decided
//! once per screen: no colour for `NO_COLOR`, `TERM=dumb` or a redirected
//! stream, and ASCII glyphs where the console font may lack them (a legacy
//! Windows console, or a non-UTF-8 POSIX locale). Body text wraps at the
//! terminal width; [`Fit::Elide`] values (paths) shorten in the middle instead.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{physical_rows, visible_rows, Frame, Note, Row, View};

/// Indent of every line the prompt layout draws.
const INDENT: &str = "  ";
/// Columns between a field's key and its value.
const FIELD_GAP: usize = 2;
/// A value column narrower than this wraps under the key instead.
const MIN_VALUE_WIDTH: usize = 16;

/// How a [`View`] is drawn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Theme {
    /// The original `> [x] Label` layout.
    #[default]
    Classic,
    /// The aligned prompt layout described in the module docs.
    Prompt(Style),
}

/// Whether a prompt screen may use colour and non-ASCII glyphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
    pub unicode: bool,
}

impl Style {
    /// No escape sequences and ASCII glyphs only.
    pub const PLAIN: Self = Self {
        color: false,
        unicode: false,
    };

    /// The style for a screen drawn on stderr.
    pub fn detect() -> Self {
        use std::io::IsTerminal;
        Self::decide(
            std::io::stderr().is_terminal(),
            crate::platform::host::WINDOWS,
            |name| std::env::var(name).ok(),
        )
    }

    /// [`Style::detect`] without the environment, for tests.
    ///
    /// `NO_COLOR` disables colour whenever it is set, matching clud's other
    /// coloured output. Glyphs need a font that has them: Windows Terminal and
    /// VS Code do, the legacy console host's raster and default fonts may not,
    /// and a POSIX terminal must run a UTF-8 locale.
    pub fn decide(terminal: bool, windows: bool, var: impl Fn(&str) -> Option<String>) -> Self {
        let set = |name: &str| var(name).filter(|value| !value.is_empty());
        let color = terminal && var("NO_COLOR").is_none() && set("TERM").as_deref() != Some("dumb");
        let unicode = terminal
            && if windows {
                set("WT_SESSION").is_some() || set("TERM_PROGRAM").as_deref() == Some("vscode")
            } else {
                ["LC_ALL", "LC_CTYPE", "LANG"]
                    .into_iter()
                    .find_map(set)
                    .is_some_and(|locale| {
                        let locale = locale.to_ascii_lowercase();
                        locale.contains("utf-8") || locale.contains("utf8")
                    })
            };
        Self { color, unicode }
    }

    /// The marker in front of the current row.
    pub fn pointer(self) -> &'static str {
        if self.unicode {
            "\u{276f}"
        } else {
            ">"
        }
    }

    /// The mark for a finished step.
    pub fn check(self) -> &'static str {
        if self.unicode {
            "\u{2713}"
        } else {
            "*"
        }
    }

    fn ellipsis(self) -> &'static str {
        if self.unicode {
            "\u{2026}"
        } else {
            "..."
        }
    }

    /// `text` in `tone`, or `text` alone without colour. For lines printed
    /// outside a selector, such as the installer's closing summary.
    pub fn paint(self, text: &str, tone: Tone) -> String {
        let code = tone.sgr();
        if self.color && !code.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

/// The emphasis of a piece of text. Meaning never rests on tone alone: it is
/// dropped entirely without colour. Text stays in the terminal's default
/// foreground, which every theme keeps readable; faint text and fixed hues
/// can vanish on a light or a dark theme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tone {
    #[default]
    Normal,
    /// Titles, headings and the current row.
    Strong,
    /// A finished step's mark, next to words that say the same.
    Good,
}

impl Tone {
    fn sgr(self) -> &'static str {
        match self {
            Tone::Normal => "",
            Tone::Strong => "1",
            Tone::Good => "32",
        }
    }
}

/// How a field value longer than its column is fitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Wrap onto further lines aligned under the value column.
    Wrap,
    /// Stay on one line, shortened in the middle (for paths).
    Elide,
    /// Always on its own lines under the key, for long audit values.
    Below,
}

/// Content between the hints and the option rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Blank,
    /// A paragraph that wraps at the terminal width.
    Text(String, Tone),
    /// `key  value`; keys align across every field of the view.
    Field {
        key: String,
        value: String,
        fit: Fit,
        tone: Tone,
    },
}

impl Block {
    pub fn field(key: &str, value: impl Into<String>) -> Self {
        Self::Field {
            key: key.into(),
            value: value.into(),
            fit: Fit::Wrap,
            tone: Tone::Normal,
        }
    }

    /// A long value printed under its key.
    pub fn below(key: &str, value: impl Into<String>) -> Self {
        Self::Field {
            key: key.into(),
            value: value.into(),
            fit: Fit::Below,
            tone: Tone::Normal,
        }
    }

    /// A field whose value is a path: shortened in the middle, never wrapped.
    pub fn path(key: &str, value: impl Into<String>) -> Self {
        Self::Field {
            key: key.into(),
            value: value.into(),
            fit: Fit::Elide,
            tone: Tone::Normal,
        }
    }
}

/// One output line: styled segments without control characters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Line(Vec<(String, Tone)>);

impl Line {
    fn of(text: impl Into<String>, tone: Tone) -> Self {
        Self(vec![(text.into(), tone)])
    }

    /// `text` after the layout indent; empty text is an empty line, so no
    /// line ends in spaces.
    fn indented(text: impl Into<String>, tone: Tone) -> Self {
        let text = text.into();
        if text.is_empty() {
            return Self::default();
        }
        Self(vec![(INDENT.into(), Tone::Normal), (text, tone)])
    }

    fn push(mut self, text: impl Into<String>, tone: Tone) -> Self {
        self.0.push((text.into(), tone));
        self
    }

    fn width(&self) -> usize {
        self.0.iter().map(|(text, _)| text.width()).sum()
    }
}

impl Frame {
    /// Append one styled line. Control characters are dropped, as in
    /// [`Frame::line`], and only this renderer's own escapes are written.
    fn styled(&mut self, line: &Line, color: bool) {
        let mut visible = 0;
        for (text, tone) in &line.0 {
            let clean: String = text.chars().filter(|c| !c.is_control()).collect();
            visible += clean.width();
            let code = tone.sgr();
            if color && !code.is_empty() && !clean.is_empty() {
                self.bytes
                    .extend_from_slice(format!("\x1b[{code}m{clean}\x1b[0m").as_bytes());
            } else {
                self.bytes.extend_from_slice(clean.as_bytes());
            }
        }
        self.rows += physical_rows(visible, self.columns);
        self.bytes.extend_from_slice(b"\r\n");
    }
}

/// The layout width for a terminal `columns` wide. The last column stays
/// empty: a console that wraps as soon as it is written would otherwise move
/// the cursor down a row the redraw does not count.
fn layout_width(columns: usize) -> usize {
    if columns == 0 {
        usize::MAX
    } else {
        columns.saturating_sub(1).max(1)
    }
}

/// `blocks` as finished text for printing outside a selector, one `\n` per
/// line, laid out for a terminal `columns` wide.
pub fn render_blocks(blocks: &[Block], style: Style, columns: usize) -> String {
    let mut frame = Frame::new(columns);
    for line in body_lines(blocks, style, layout_width(columns)) {
        frame.styled(&line, style.color);
    }
    frame.text().replace("\r\n", "\n")
}

/// Lay out `view` in the prompt layout; see [`super::render_within`].
pub(super) fn render(view: &View, style: Style, columns: usize, rows: usize) -> Frame {
    let width = layout_width(columns);
    let mut head = vec![Line::default(), Line::indented(&view.title, Tone::Strong)];
    for hint in &view.hints {
        head.extend(paragraph(hint, Tone::Normal, width));
    }
    if !view.body.is_empty() {
        head.push(Line::default());
        head.extend(body_lines(&view.body, style, width));
    }
    if view.gap || !view.body.is_empty() {
        head.push(Line::default());
    }
    let row_lines: Vec<Vec<Line>> = view.rows.iter().map(|row| row_lines(row, style)).collect();
    let mut tail = Vec::new();
    if !view.footer.is_empty() {
        tail.push(Line::default());
        for line in &view.footer {
            tail.extend(paragraph(line, Tone::Normal, width));
        }
    }

    let count = |lines: &[Line]| -> usize {
        lines
            .iter()
            .map(|line| physical_rows(line.width(), columns))
            .sum()
    };
    let window = if rows == 0 {
        0..row_lines.len()
    } else {
        let budget = rows
            .saturating_sub(1)
            .saturating_sub(count(&head) + count(&tail));
        let heights: Vec<usize> = row_lines.iter().map(|lines| count(lines)).collect();
        let current = view.rows.iter().position(|row| row.current).unwrap_or(0);
        visible_rows(&heights, current, budget)
    };

    let mut frame = Frame::new(columns);
    for line in &head {
        frame.styled(line, style.color);
    }
    if window.start > 0 {
        let more = format!("{INDENT}  ... {} more above", window.start);
        frame.styled(&Line::of(more, Tone::Normal), style.color);
    }
    for line in row_lines[window.clone()].iter().flatten() {
        frame.styled(line, style.color);
    }
    if window.end < row_lines.len() {
        let more = format!("{INDENT}  ... {} more below", row_lines.len() - window.end);
        frame.styled(&Line::of(more, Tone::Normal), style.color);
    }
    for line in &tail {
        frame.styled(line, style.color);
    }
    frame
}

/// The lines one option row draws: a pointer on the current row only.
fn row_lines(row: &Row, style: Style) -> Vec<Line> {
    let mut line = if row.current {
        Line::indented(format!("{} {}", style.pointer(), row.label), Tone::Strong)
    } else {
        Line::indented(format!("  {}", row.label), Tone::Normal)
    };
    match &row.note {
        Note::None => vec![line],
        Note::Inline(note) => vec![line.push(format!("  {note}"), Tone::Normal)],
        Note::Below(note) => vec![line, Line::indented(format!("    {note}"), Tone::Normal)],
        Note::Release {
            current, latest, ..
        } => {
            if *latest {
                line = line.push("  (latest)", Tone::Normal);
            }
            if *current {
                line = line.push("  (this copy)", Tone::Normal);
            }
            vec![line]
        }
    }
}

/// `text` split at newlines and wrapped to `width`, indented.
fn paragraph(text: &str, tone: Tone, width: usize) -> Vec<Line> {
    text.split('\n')
        .flat_map(|piece| wrap(piece, width.saturating_sub(INDENT.len())))
        .map(|line| Line::indented(line, tone))
        .collect()
}

/// The key column of each block: the widest key in its run of consecutive
/// fields, so expanding one group never shifts another.
fn key_widths(body: &[Block]) -> Vec<usize> {
    let mut widths = vec![0; body.len()];
    let mut start = 0;
    while start < body.len() {
        let end = body[start..]
            .iter()
            .position(|block| !matches!(block, Block::Field { .. }))
            .map_or(body.len(), |offset| start + offset);
        let widest = body[start..end]
            .iter()
            .map(|block| match block {
                Block::Field { key, .. } => key.width(),
                _ => 0,
            })
            .max()
            .unwrap_or(0);
        widths[start..end].fill(widest);
        start = end + 1;
    }
    widths
}

fn body_lines(body: &[Block], style: Style, width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    for (block, key_width) in body.iter().zip(key_widths(body)) {
        match block {
            Block::Blank => lines.push(Line::default()),
            Block::Text(text, tone) => lines.extend(paragraph(text, *tone, width)),
            Block::Field {
                key,
                value,
                fit,
                tone,
            } => lines.extend(field_lines(
                key, value, *fit, *tone, key_width, width, style,
            )),
        }
    }
    lines
}

/// One aligned field. A value column too narrow to read moves the value
/// onto its own lines under the key.
fn field_lines(
    key: &str,
    value: &str,
    fit: Fit,
    tone: Tone,
    key_width: usize,
    width: usize,
    style: Style,
) -> Vec<Line> {
    let lead = INDENT.len() + key_width + FIELD_GAP;
    let (column, stacked) = match width.checked_sub(lead) {
        Some(room) if room >= MIN_VALUE_WIDTH && fit != Fit::Below => (room, false),
        _ => (width.saturating_sub(INDENT.len() * 2).max(1), true),
    };
    let values = match fit {
        Fit::Elide => vec![elide_middle(value, column, style.ellipsis())],
        Fit::Wrap | Fit::Below => value
            .trim_end()
            .split('\n')
            .flat_map(|piece| wrap(piece, column))
            .collect(),
    };
    let key_line = Line::indented(key, Tone::Normal);
    if stacked {
        let mut lines = vec![key_line];
        lines.extend(
            values
                .into_iter()
                .map(|value| Line::indented(format!("{INDENT}{value}"), tone)),
        );
        return lines;
    }
    let pad = " ".repeat(key_width - key.width() + FIELD_GAP);
    let mut values = values.into_iter();
    let first = values.next().unwrap_or_default();
    let mut lines = vec![key_line.push(pad, Tone::Normal).push(first, tone)];
    lines.extend(values.map(|value| Line::of(format!("{}{value}", " ".repeat(lead)), tone)));
    lines
}

/// Word-wrap `text` to `width` columns, breaking inside a word only when the
/// word alone is wider than a line. `usize::MAX` (unknown width) never wraps.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let needed = if line.is_empty() {
            word.width()
        } else {
            line.width() + 1 + word.width()
        };
        if needed <= width {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        let mut rest = word;
        while rest.width() > width {
            let (head, tail) = split_long_word(rest, width);
            lines.push(head.to_owned());
            rest = tail;
        }
        line.push_str(rest);
    }
    lines.push(line);
    lines
}

/// Split a word wider than `width`: after the last path or list separator
/// that fits, else at the width.
fn split_long_word(word: &str, width: usize) -> (&str, &str) {
    let mut fit = 0;
    let mut used = 0;
    let mut separator = None;
    for (index, c) in word.char_indices() {
        used += c.width().unwrap_or(0);
        if used > width {
            break;
        }
        fit = index + c.len_utf8();
        if matches!(c, '\\' | '/' | ';' | ',') {
            separator = Some(fit);
        }
    }
    let at = separator.unwrap_or(fit.max(word.chars().next().map_or(0, char::len_utf8)));
    word.split_at(at)
}

/// `text` shortened to `width` columns by replacing its middle with
/// `ellipsis`, keeping more of the end (a path's file name). Cuts fall on
/// path separators when one is in reach, so no directory name is split:
/// `C:\Users\me\...\clud\bin\clud.exe`.
pub fn elide_middle(text: &str, width: usize, ellipsis: &str) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let room = width.saturating_sub(ellipsis.width());
    let head = prefix_within(text, room / 3);
    let head = match head.rfind(['\\', '/']) {
        Some(at) if at > 0 => &head[..=at],
        _ => head,
    };
    let tail = suffix_within(text, room - head.width());
    let tail = match tail.find(['\\', '/']) {
        Some(at) if at + 1 < tail.len() => &tail[at..],
        _ => tail,
    };
    format!("{head}{ellipsis}{tail}")
}

/// The longest prefix of `text` at most `width` columns wide.
fn prefix_within(text: &str, width: usize) -> &str {
    let mut used = 0;
    for (index, c) in text.char_indices() {
        used += c.width().unwrap_or(0);
        if used > width {
            return &text[..index];
        }
    }
    text
}

/// The longest suffix of `text` at most `width` columns wide.
fn suffix_within(text: &str, width: usize) -> &str {
    let mut used = 0;
    for (index, c) in text.char_indices().rev() {
        used += c.width().unwrap_or(0);
        if used > width {
            return &text[index + c.len_utf8()..];
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn style_honours_no_color_dumb_terminals_and_redirection() {
        let utf8 = [("LANG", "en_US.UTF-8")];
        assert_eq!(
            Style::decide(true, false, env(&utf8)),
            Style {
                color: true,
                unicode: true
            }
        );
        assert!(!Style::decide(true, false, env(&[("NO_COLOR", "")])).color);
        assert!(!Style::decide(true, false, env(&[("TERM", "dumb")])).color);
        assert_eq!(Style::decide(false, false, env(&utf8)), Style::PLAIN);
    }

    #[test]
    fn glyphs_need_a_capable_console() {
        assert!(!Style::decide(true, false, env(&[("LANG", "C")])).unicode);
        assert!(!Style::decide(true, false, env(&[])).unicode);
        assert!(Style::decide(true, false, env(&[("LC_ALL", "C.utf8"), ("LANG", "C")])).unicode);
        // The legacy Windows console stays ASCII; Windows Terminal does not.
        assert!(!Style::decide(true, true, env(&[("LANG", "en_US.UTF-8")])).unicode);
        assert!(Style::decide(true, true, env(&[("WT_SESSION", "abc")])).unicode);
        assert_eq!(Style::PLAIN.pointer(), ">");
    }

    #[test]
    fn wrap_breaks_on_words_and_splits_only_overlong_words() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(
            wrap(r"C:\Users\me\AppData;D:\bin x", 14),
            [r"C:\Users\me\", r"AppData;D:\bin", "x"]
        );
        assert_eq!(wrap("short", usize::MAX), ["short"]);
        assert_eq!(wrap("", 10), [""]);
    }

    #[test]
    fn elide_keeps_the_end_of_a_path() {
        let path = "/home/someone/.local/share/very/long/bin/clud";
        let short = elide_middle(path, 24, "...");
        assert!(short.width() <= 24, "{short}");
        assert_eq!(short, "/home/.../long/bin/clud");
        assert_eq!(
            elide_middle(
                r"C:\Users\vpkmc\AppData\Local\Programs\clud\bin\clud.exe",
                45,
                "..."
            ),
            r"C:\Users\...\Local\Programs\clud\bin\clud.exe"
        );
        // No separator in reach: a plain middle cut.
        assert_eq!(elide_middle("abcdefghijklmnop", 9, "..."), "ab...mnop");
        assert_eq!(elide_middle("fits", 10, "..."), "fits");
    }

    fn screen(style: Style) -> View {
        View {
            title: "Install clud 1.2.3".into(),
            body: vec![
                Block::field("Version", "1.2.3"),
                Block::path(
                    "Location",
                    "C:\\Users\\someone\\AppData\\Local\\Programs\\clud\\bin\\clud.exe",
                ),
                Block::field("SHA-256", "0123456789abcdef".repeat(4)),
            ],
            rows: ["Install", "Not now"]
                .iter()
                .enumerate()
                .map(|(index, label)| Row {
                    current: index == 1,
                    marker: String::new(),
                    label: (*label).into(),
                    note: Note::None,
                })
                .collect(),
            footer: vec!["Up/Down move, Enter select, Esc cancel".into()],
            theme: Theme::Prompt(style),
            ..View::default()
        }
    }

    #[test]
    fn every_line_fits_the_terminal_width() {
        for columns in [40, 60, 80, 120] {
            let frame = super::super::render(&screen(Style::PLAIN), columns);
            let text = frame.text();
            for line in text.split("\r\n") {
                assert!(line.width() <= columns, "{columns}: {line:?}");
                assert_eq!(line, line.trim_end(), "trailing space: {line:?}");
            }
            assert_eq!(frame.rows(), text.matches("\r\n").count());
        }
    }

    #[test]
    fn plain_style_writes_no_escapes_and_marks_the_current_row_in_text() {
        let text = super::super::render(&screen(Style::PLAIN), 80).text();
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(text.contains("  > Not now\r\n"), "{text}");
        assert!(text.contains("    Install\r\n"), "{text}");
        assert!(text.contains("  Version   1.2.3\r\n"), "{text}");
    }

    #[test]
    fn colour_style_wraps_only_its_own_segments() {
        let style = Style {
            color: true,
            unicode: true,
        };
        let text = super::super::render(&screen(style), 80).text();
        assert!(text.contains("\x1b[1m\u{276f} Not now\x1b[0m"), "{text:?}");
        assert!(
            text.contains("\x1b[1mInstall clud 1.2.3\x1b[0m"),
            "{text:?}"
        );
        assert!(!text.contains("\x1b[2m"), "no faint text: {text:?}");
    }

    #[test]
    fn narrow_terminals_stack_values_under_keys() {
        let text = super::super::render(&screen(Style::PLAIN), 24).text();
        assert!(text.contains("  Location\r\n    "), "{text}");
    }
}

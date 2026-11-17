//! Installer screens rendered through the shared terminal selector in its
//! prompt layout (`selector::Theme::Prompt`).
//!
//! Each decision is one screen: a title naming the version, the plan in plain
//! words ([`super::summary`]), and one single-choice list. **Show details**
//! prints the exact plan into scrollback and reopens the screen below it, so
//! a long plan never makes the screen taller than the terminal. The list order is part of the interface:
//! automation and muscle memory press Down/Up from the default row, so new
//! rows go after **Not now**.

use std::time::Duration;

use crate::selector::{self, Block, Key, Note, OnExit, Row, Selector, Step, Style, Theme, View};

use super::catalog::ResolvedRelease;
use super::summary::{Facts, PathStyle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuChoice {
    Current,
    Releases,
    Browser,
    NotNow,
    Cancelled,
}

/// One row of an installer screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Install,
    Repair,
    Releases,
    Browser,
    NotNow,
    Details,
}

/// A titled plan summary with one single-choice list.
pub struct Screen {
    title: String,
    hints: Vec<String>,
    summary: Vec<Block>,
    details: Vec<Block>,
    actions: Vec<Action>,
    selected: usize,
    /// Set when **Show details** closes the screen, which then erases itself
    /// so the details print where it stood and it reopens below them.
    details_requested: bool,
    style: Style,
}

impl Screen {
    /// The first screen of `--installer` and the first-run offer: install
    /// this executable, pick another release, open the download page, or
    /// decline. The offer defaults to **Not now**. `facts` is `None` when the
    /// plan could not be read; choosing **Install** then reports why.
    pub fn menu(facts: Option<&Facts>, paths: &PathStyle, automatic: bool, style: Style) -> Self {
        let mut actions = vec![
            Action::Install,
            Action::Releases,
            Action::Browser,
            Action::NotNow,
        ];
        if facts.is_some() {
            actions.push(Action::Details);
        }
        Self {
            title: facts.map_or_else(
                || format!("Install clud {}", env!("CARGO_PKG_VERSION")),
                Facts::title,
            ),
            hints: automatic
                .then(|| "clud is not set up as a command yet.".to_owned())
                .into_iter()
                .chain(facts.map(|facts| facts.subtitle().to_owned()))
                .collect(),
            summary: facts.map(|facts| facts.summary(paths)).unwrap_or_default(),
            details: facts.map(Facts::details).unwrap_or_default(),
            actions,
            selected: if automatic { 3 } else { 0 },
            details_requested: false,
            style,
        }
    }

    /// Consent for one planned install. Defaults to **Not now**.
    pub fn confirm(facts: &Facts, paths: &PathStyle, style: Style) -> Self {
        Self {
            title: facts.title(),
            hints: vec![facts.subtitle().to_owned()],
            summary: facts.summary(paths),
            details: facts.details(),
            actions: vec![Action::Install, Action::NotNow, Action::Details],
            selected: 1,
            details_requested: false,
            style,
        }
    }

    /// Separate consent for one more activation attempt. Defaults to
    /// **Not now**.
    pub fn repair(title: String, style: Style) -> Self {
        Self {
            title,
            hints: vec![
                "clud is installed, but new terminals cannot find it yet.".into(),
                "Retry once? Nothing changes unless you choose to retry.".into(),
            ],
            summary: Vec::new(),
            details: Vec::new(),
            actions: vec![Action::Repair, Action::NotNow],
            selected: 1,
            details_requested: false,
            style,
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The exact plan as text to print above the reopened screen.
    pub fn details_text(&self, columns: usize) -> String {
        selector::render_blocks(&self.details, self.style, columns)
    }

    /// Reopen after the details were printed: they stay in scrollback, so
    /// the row that showed them goes.
    fn details_shown(&mut self) {
        self.details_requested = false;
        self.actions.retain(|action| *action != Action::Details);
        self.selected = self.selected.min(self.actions.len() - 1);
    }

    fn label(action: Action) -> &'static str {
        match action {
            Action::Install => "Install",
            Action::Repair => "Retry command setup",
            Action::Releases => "Choose another version",
            Action::Browser => "Open download page",
            Action::NotNow => "Not now",
            Action::Details => "Show details",
        }
    }
}

/// Run `screen` until it closes with a choice, printing the details above it
/// each time they are requested.
fn run_screen(mut screen: Screen) -> std::io::Result<MenuChoice> {
    loop {
        if let Some(choice) = selector::run(&mut std::io::stderr(), &mut screen)? {
            return Ok(choice);
        }
        eprint!("\n{}", screen.details_text(selector::terminal_columns()));
        screen.details_shown();
    }
}

/// The key hint under every installer list.
pub fn hint(style: Style) -> String {
    if style.unicode {
        "\u{2191}/\u{2193} move \u{b7} Enter select \u{b7} Esc cancel".into()
    } else {
        "Up/Down move, Enter select, Esc cancel".into()
    }
}

fn rows(labels: impl Iterator<Item = String>, selected: usize) -> Vec<Row> {
    labels
        .enumerate()
        .map(|(index, label)| Row {
            current: index == selected,
            marker: String::new(),
            label,
            note: Note::None,
        })
        .collect()
}

impl Selector for Screen {
    /// `None` asks for the details.
    type Outcome = Option<MenuChoice>;

    fn view(&self, _elapsed: Duration) -> View {
        View {
            title: self.title.clone(),
            hints: self.hints.clone(),
            body: self.summary.clone(),
            gap: true,
            rows: rows(
                self.actions
                    .iter()
                    .map(|action| Self::label(*action).to_owned()),
                self.selected,
            ),
            footer: vec![hint(self.style)],
            theme: Theme::Prompt(self.style),
        }
    }

    fn on_key(&mut self, key: Key) -> Step<Self::Outcome> {
        match key {
            Key::Up => {
                self.selected = self.selected.saturating_sub(1);
                Step::Redraw
            }
            Key::Down => {
                self.selected = (self.selected + 1).min(self.actions.len() - 1);
                Step::Redraw
            }
            Key::Enter => Step::Done(match self.actions[self.selected] {
                Action::Install | Action::Repair => Some(MenuChoice::Current),
                Action::Releases => Some(MenuChoice::Releases),
                Action::Browser => Some(MenuChoice::Browser),
                Action::NotNow => Some(MenuChoice::NotNow),
                Action::Details => {
                    self.details_requested = true;
                    None
                }
            }),
            Key::Escape => Step::Done(Some(MenuChoice::Cancelled)),
            Key::Space | Key::Char(_) => Step::Stay,
        }
    }

    fn on_exit(&self) -> OnExit {
        if self.details_requested {
            OnExit::Erase
        } else {
            OnExit::Keep
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseChoice {
    Selected(ResolvedRelease),
    Cancelled,
}

pub struct Releases {
    rows: Vec<ResolvedRelease>,
    latest_stable: String,
    selected: usize,
    style: Style,
}

impl Releases {
    pub fn new(rows: Vec<ResolvedRelease>, latest_stable: &str, style: Style) -> Self {
        let selected = rows
            .iter()
            .position(|row| row.version == latest_stable)
            .unwrap_or(0);
        Self {
            rows,
            latest_stable: latest_stable.into(),
            selected,
            style,
        }
    }

    pub fn selected(&self) -> Option<&ResolvedRelease> {
        self.rows.get(self.selected)
    }
}

impl Selector for Releases {
    type Outcome = ReleaseChoice;

    fn view(&self, _elapsed: Duration) -> View {
        View {
            title: "Choose a clud release".into(),
            hints: vec![
                format!(
                    "This copy: {}   Latest: {}",
                    env!("CARGO_PKG_VERSION"),
                    self.latest_stable
                ),
                "You review the version you pick before it installs.".into(),
            ],
            gap: true,
            rows: self
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| Row {
                    current: self.selected == index,
                    marker: String::new(),
                    label: format!("clud {}", row.version),
                    note: Note::Release {
                        asset: row.asset.filename.to_string(),
                        current: row.version == env!("CARGO_PKG_VERSION"),
                        latest: row.version == self.latest_stable,
                    },
                })
                .collect(),
            footer: vec![hint(self.style)],
            theme: Theme::Prompt(self.style),
            ..View::default()
        }
    }

    fn on_key(&mut self, key: Key) -> Step<Self::Outcome> {
        match key {
            Key::Up => {
                self.selected = self.selected.saturating_sub(1);
                Step::Redraw
            }
            Key::Down => {
                self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1));
                Step::Redraw
            }
            Key::Enter => self
                .selected()
                .cloned()
                .map(ReleaseChoice::Selected)
                .map_or(Step::Stay, Step::Done),
            Key::Escape => Step::Done(ReleaseChoice::Cancelled),
            Key::Space | Key::Char(_) => Step::Stay,
        }
    }
}

pub fn prompt_menu(facts: Option<&Facts>, automatic: bool) -> std::io::Result<MenuChoice> {
    run_screen(Screen::menu(
        facts,
        &PathStyle::detect(),
        automatic,
        Style::detect(),
    ))
}

pub fn prompt_releases(
    rows: Vec<ResolvedRelease>,
    latest_stable: &str,
) -> std::io::Result<ReleaseChoice> {
    selector::run(
        &mut std::io::stderr(),
        &mut Releases::new(rows, latest_stable, Style::detect()),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmChoice {
    Proceed,
    NotNow,
    Cancelled,
}

impl From<MenuChoice> for ConfirmChoice {
    fn from(choice: MenuChoice) -> Self {
        match choice {
            MenuChoice::Current => Self::Proceed,
            MenuChoice::Cancelled => Self::Cancelled,
            MenuChoice::Releases | MenuChoice::Browser | MenuChoice::NotNow => Self::NotNow,
        }
    }
}

pub fn prompt_confirm(facts: &Facts) -> std::io::Result<ConfirmChoice> {
    run_screen(Screen::confirm(
        facts,
        &PathStyle::detect(),
        Style::detect(),
    ))
    .map(ConfirmChoice::from)
}

pub fn prompt_repair(label: String) -> std::io::Result<ConfirmChoice> {
    run_screen(Screen::repair(label, Style::detect())).map(ConfirmChoice::from)
}

#[cfg(test)]
#[path = "picker_tests.rs"]
mod tests;

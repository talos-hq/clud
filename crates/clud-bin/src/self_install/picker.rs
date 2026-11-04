//! Installer menu models rendered through the shared terminal selector.

use std::time::Duration;

use crate::selector::{self, Key, Note, Row, Selector, Step, View};

use super::catalog::ResolvedRelease;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuChoice {
    Current,
    Releases,
    Browser,
    NotNow,
    Cancelled,
}

pub struct Menu {
    selected: usize,
}

impl Menu {
    pub fn new(automatic: bool) -> Self {
        Self {
            selected: if automatic { 3 } else { 0 },
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }
}

impl Selector for Menu {
    type Outcome = MenuChoice;

    fn view(&self, _elapsed: Duration) -> View {
        let labels = [
            "Install this version",
            "Choose a release",
            "Open download page",
            "Not now",
        ];
        View {
            title: "Install clud".into(),
            hints: vec!["Up/Down choose, Enter confirm, Esc cancel".into()],
            gap: true,
            rows: labels
                .iter()
                .enumerate()
                .map(|(index, label)| Row {
                    current: self.selected == index,
                    marker: if self.selected == index { "[x]" } else { "[ ]" }.into(),
                    label: (*label).into(),
                    note: Note::None,
                })
                .collect(),
            footer: Vec::new(),
        }
    }

    fn on_key(&mut self, key: Key) -> Step<Self::Outcome> {
        match key {
            Key::Up => {
                self.selected = self.selected.saturating_sub(1);
                Step::Redraw
            }
            Key::Down => {
                self.selected = (self.selected + 1).min(3);
                Step::Redraw
            }
            Key::Enter => Step::Done(
                [
                    MenuChoice::Current,
                    MenuChoice::Releases,
                    MenuChoice::Browser,
                    MenuChoice::NotNow,
                ][self.selected],
            ),
            Key::Escape => Step::Done(MenuChoice::Cancelled),
            Key::Space | Key::Char(_) => Step::Stay,
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
}

impl Releases {
    pub fn new(rows: Vec<ResolvedRelease>, latest_stable: &str) -> Self {
        let selected = rows
            .iter()
            .position(|row| row.version == latest_stable)
            .unwrap_or(0);
        Self {
            rows,
            latest_stable: latest_stable.into(),
            selected,
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
                    "Current: {}   Latest: {}",
                    env!("CARGO_PKG_VERSION"),
                    self.latest_stable
                ),
                "Up/Down scroll, Enter select, Esc cancel".into(),
            ],
            gap: true,
            rows: self
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| Row {
                    current: self.selected == index,
                    marker: if self.selected == index { "[x]" } else { "[ ]" }.into(),
                    label: format!("clud {}", row.version),
                    note: Note::Release {
                        asset: row.asset.filename.to_string(),
                        current: row.version == env!("CARGO_PKG_VERSION"),
                        latest: row.version == self.latest_stable,
                    },
                })
                .collect(),
            footer: Vec::new(),
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

pub fn prompt_menu(automatic: bool) -> std::io::Result<MenuChoice> {
    selector::run(&mut std::io::stderr(), &mut Menu::new(automatic))
}

pub fn prompt_releases(
    rows: Vec<ResolvedRelease>,
    latest_stable: &str,
) -> std::io::Result<ReleaseChoice> {
    selector::run(
        &mut std::io::stderr(),
        &mut Releases::new(rows, latest_stable),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmChoice {
    Proceed,
    NotNow,
    Cancelled,
}

pub struct Confirm {
    label: String,
    action: &'static str,
    selected: usize,
}

impl Confirm {
    pub fn new(label: String) -> Self {
        Self::with_action(label, "Install")
    }

    fn with_action(label: String, action: &'static str) -> Self {
        Self {
            label,
            action,
            selected: 1,
        }
    }
}

impl Selector for Confirm {
    type Outcome = ConfirmChoice;

    fn view(&self, _elapsed: Duration) -> View {
        View {
            title: self.label.clone(),
            hints: vec!["Up/Down choose, Enter confirm, Esc cancel".into()],
            gap: true,
            rows: [self.action, "Not now"]
                .iter()
                .enumerate()
                .map(|(index, label)| Row {
                    current: self.selected == index,
                    marker: if self.selected == index { "[x]" } else { "[ ]" }.into(),
                    label: (*label).into(),
                    note: Note::None,
                })
                .collect(),
            footer: Vec::new(),
        }
    }

    fn on_key(&mut self, key: Key) -> Step<Self::Outcome> {
        match key {
            Key::Up => {
                self.selected = 0;
                Step::Redraw
            }
            Key::Down => {
                self.selected = 1;
                Step::Redraw
            }
            Key::Enter => Step::Done(if self.selected == 0 {
                ConfirmChoice::Proceed
            } else {
                ConfirmChoice::NotNow
            }),
            Key::Escape => Step::Done(ConfirmChoice::Cancelled),
            Key::Space | Key::Char(_) => Step::Stay,
        }
    }
}

pub fn prompt_confirm(label: String) -> std::io::Result<ConfirmChoice> {
    selector::run(&mut std::io::stderr(), &mut Confirm::new(label))
}

pub fn prompt_repair(label: String) -> std::io::Result<ConfirmChoice> {
    selector::run(
        &mut std::io::stderr(),
        &mut Confirm::with_action(label, "Repair"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_install::catalog::{Arch, Flavor, MediaType, Os, ResolvedAsset};

    fn row(version: &str) -> ResolvedRelease {
        ResolvedRelease {
            version: version.into(),
            asset: ResolvedAsset {
                version: version.into(),
                filename: format!("clud-{version}-x86_64-unknown-linux-musl"),
                media_type: MediaType::Direct,
                size_bytes: 1,
                sha256: "0".repeat(64),
                url: String::new(),
                flavor: Flavor::StaticMusl,
                os: Os::Linux,
                arch: Arch::X86_64,
            },
        }
    }

    #[test]
    fn automatic_defaults_to_not_now_and_explicit_to_current() {
        let mut auto = Menu::new(true);
        assert_eq!(auto.selected(), 3);
        assert_eq!(auto.on_key(Key::Enter), Step::Done(MenuChoice::NotNow));
        assert_eq!(
            Menu::new(false).on_key(Key::Enter),
            Step::Done(MenuChoice::Current)
        );
        assert_eq!(auto.on_key(Key::Escape), Step::Done(MenuChoice::Cancelled));
        let mut confirm = Confirm::new("Install clud?".into());
        assert_eq!(
            confirm.on_key(Key::Enter),
            Step::Done(ConfirmChoice::NotNow)
        );
        assert_eq!(confirm.on_key(Key::Up), Step::Redraw);
        assert_eq!(
            confirm.on_key(Key::Enter),
            Step::Done(ConfirmChoice::Proceed)
        );
    }

    #[test]
    fn release_markers_show_both_versions_and_render_trusted_colors() {
        let current = env!("CARGO_PKG_VERSION");
        for latest in [current, "99.0.0"] {
            let picker = Releases::new(vec![row(current), row("99.0.0")], latest);
            let view = picker.view(Duration::ZERO);
            assert!(view.hints[0].contains(current));
            assert!(view.hints[0].contains(latest));
            let rendered = selector::render(&view, 200).text();
            assert!(rendered.contains("\x1b[33m<-- current\x1b[0m"));
            assert!(rendered.contains("\x1b[32m<-- latest\x1b[0m"));
            assert_eq!(rendered.matches("<-- current").count(), 1);
            assert_eq!(rendered.matches("<-- latest").count(), 1);
        }
        let picker = Releases::new(vec![row("99.0.0")], "99.0.0");
        assert!(!selector::render(&picker.view(Duration::ZERO), 200)
            .text()
            .contains("<-- current"));
    }

    #[test]
    fn release_picker_defaults_to_stable_pointer_and_keeps_exact_choice() {
        let mut picker = Releases::new(
            vec![row("3.0.0-rc.1"), row("2.9.0"), row("2.8.14")],
            "2.9.0",
        );
        assert_eq!(picker.selected().unwrap().version, "2.9.0");
        assert!(matches!(picker.on_key(Key::Down), Step::Redraw));
        assert!(
            matches!(picker.on_key(Key::Enter), Step::Done(ReleaseChoice::Selected(ref selected)) if selected.version == "2.8.14")
        );
        let frame = selector::render_within(&picker.view(Duration::ZERO), 24, 7);
        assert!(frame.text().contains("more above") || frame.text().contains("more below"));
        assert!(frame
            .bytes()
            .iter()
            .enumerate()
            .all(|(i, byte)| *byte != b'\n' || (i > 0 && frame.bytes()[i - 1] == b'\r')));
    }
}

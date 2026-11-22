//! Provider selection, before any credential values are copied.

use std::io::Write;
use std::time::Duration;

use crate::selector::{self, Key, Note, Row, Selector, Step, View};

/// Clap and the command runner share validation, before fetching or writing.
pub fn provider(value: &str) -> Result<String, String> {
    if value == super::configure::CODEX_GATEWAY_ID
        || crate::provider_registry::ANTHROPIC_COMPAT_PROVIDERS
            .iter()
            .any(|descriptor| descriptor.settings_id == value)
    {
        Ok(value.to_string())
    } else {
        Err("expected a stored provider id: openrouter, deepseek, kimi or clud-codex".into())
    }
}

pub fn choose(
    available: &[String],
    requested: &[String],
    migrate_all: bool,
    yes: bool,
    interactive: bool,
    mut out: &mut dyn Write,
) -> Result<Vec<String>, String> {
    if !requested.is_empty() {
        for id in requested {
            provider(id)?;
            if !available.contains(id) {
                return Err(format!(
                    "{id} has no available credential in clud; nothing migrated"
                ));
            }
        }
        return Ok(available
            .iter()
            .filter(|id| requested.contains(id))
            .cloned()
            .collect());
    }
    if migrate_all {
        return Ok(available.to_vec());
    }
    if yes || !interactive || available.is_empty() {
        return Ok(Vec::new());
    }
    let mut picker = Providers::new(available);
    selector::run(&mut out, &mut picker)
        .map_err(|error| format!("provider selection cancelled: {error}"))
}

struct Providers {
    ids: Vec<String>,
    checked: Vec<bool>,
    cursor: usize,
}

impl Providers {
    fn new(ids: &[String]) -> Self {
        Self {
            ids: ids.to_vec(),
            checked: vec![false; ids.len()],
            cursor: 0,
        }
    }
}

impl Selector for Providers {
    type Outcome = Vec<String>;

    fn view(&self, _elapsed: Duration) -> View {
        View {
            title: "Choose credentials to migrate".into(),
            hints: vec!["Up/Down moves; Space toggles; Enter applies; Esc cancels. None selected by default.".into()],
            rows: self.ids.iter().enumerate().map(|(index, id)| Row {
                current: index == self.cursor,
                marker: selector::check_marker(self.checked[index]).into(),
                label: id.clone(),
                note: if id == super::configure::CODEX_GATEWAY_ID {
                    Note::Below("Also enables the local Codex gateway at login on Linux.".into())
                } else { Note::None },
            }).collect(),
            ..View::default()
        }
    }

    fn on_key(&mut self, key: Key) -> Step<Self::Outcome> {
        match key {
            Key::Up => self.cursor = self.cursor.saturating_sub(1),
            Key::Down => self.cursor = (self.cursor + 1).min(self.ids.len().saturating_sub(1)),
            Key::Space if !self.ids.is_empty() => {
                self.checked[self.cursor] = !self.checked[self.cursor]
            }
            Key::Enter => {
                return Step::Done(
                    self.ids
                        .iter()
                        .zip(&self.checked)
                        .filter(|(_, checked)| **checked)
                        .map(|(id, _)| id.clone())
                        .collect(),
                )
            }
            Key::Escape => return Step::Done(Vec::new()),
            _ => return Step::Stay,
        }
        Step::Redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn available() -> Vec<String> {
        vec!["openrouter".into(), "clud-codex".into()]
    }

    #[test]
    fn migration_choice_keeps_yes_opt_in_and_migrate_all_semantics() {
        let all = available();
        assert!(choose(&all, &[], false, true, true, &mut Vec::new())
            .unwrap()
            .is_empty());
        assert!(choose(&all, &[], false, false, false, &mut Vec::new())
            .unwrap()
            .is_empty());
        assert_eq!(
            choose(&all, &[], true, true, false, &mut Vec::new()).unwrap(),
            all
        );
    }

    #[test]
    fn migration_choice_explicit_subset_wins_over_migrate_all_and_is_deduplicated() {
        let wanted = vec!["clud-codex".into(), "clud-codex".into()];
        for migrate_all in [false, true] {
            assert_eq!(
                choose(
                    &available(),
                    &wanted,
                    migrate_all,
                    true,
                    false,
                    &mut Vec::new()
                )
                .unwrap(),
                ["clud-codex"]
            );
        }
        assert!(choose(
            &available(),
            &["kimi".into()],
            true,
            true,
            false,
            &mut Vec::new()
        )
        .is_err());
        assert!(provider("typo").is_err());
    }

    #[test]
    fn interactive_migration_choice_can_select_only_codex_or_cancel() {
        let mut picker = Providers::new(&available());
        assert_eq!(picker.on_key(Key::Enter), Step::Done(Vec::new()));
        picker.on_key(Key::Down);
        picker.on_key(Key::Space);
        assert_eq!(
            picker.on_key(Key::Enter),
            Step::Done(vec!["clud-codex".into()])
        );
        assert_eq!(picker.on_key(Key::Escape), Step::Done(Vec::new()));
    }
}

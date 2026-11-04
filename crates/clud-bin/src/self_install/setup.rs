//! Provider setup (#2219, from #2223): pick OpenRouter, Claude or Codex,
//! store an OpenRouter key and model, and persist the provider with the
//! native harness. `clud setup` ([`crate::setup_command`]) is its entry
//! point; the native installer reaches it through
//! [`crate::setup_command::run_provider_step_interactive`].

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use crate::args::DeepseekAuthSubcommand;
use crate::backend::{HarnessSelection, ModelProvider};
use crate::selector::{self, Key, Note, Row, Selector, Step, View};

const DEEPSEEK: &str = "deepseek/deepseek-v4.1-flash";
const GLM: &str = "z-ai/glm-5.3-flash";

/// The providers setup offers, in menu order. A [`ModelProvider`] missing
/// here must be listed in [`NOT_OFFERED`]; a unit test enforces it.
pub const OFFERED: [ModelProvider; 3] = [
    ModelProvider::OpenRouter,
    ModelProvider::Claude,
    ModelProvider::Codex,
];

/// Providers deliberately left out of setup: they stay reachable through
/// their launch flags and `clud auth login`.
pub const NOT_OFFERED: &[ModelProvider] = &[ModelProvider::DeepSeek, ModelProvider::Kimi];

const PROVIDER_LABELS: &[&str] = &["OpenRouter", "Claude", "Codex"];

struct Choice {
    title: &'static str,
    labels: &'static [&'static str],
    selected: usize,
}

impl Selector for Choice {
    type Outcome = Option<usize>;

    fn view(&self, _: Duration) -> View {
        View {
            title: self.title.into(),
            hints: vec!["Up/Down choose, Enter confirm, Esc cancel".into()],
            gap: true,
            rows: self
                .labels
                .iter()
                .enumerate()
                .map(|(index, label)| Row {
                    current: self.selected == index,
                    marker: if self.selected == index { "[*]" } else { "[ ]" }.into(),
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
                self.selected = (self.selected + 1).min(self.labels.len() - 1);
                Step::Redraw
            }
            Key::Enter => Step::Done(Some(self.selected)),
            Key::Escape => Step::Done(None),
            Key::Space | Key::Char(_) => Step::Stay,
        }
    }
}

fn choose(title: &'static str, labels: &'static [&'static str]) -> Result<usize, i32> {
    selector::run(
        &mut io::stderr(),
        &mut Choice {
            title,
            labels,
            selected: 0,
        },
    )
    .map_err(|error| {
        if error.kind() == io::ErrorKind::Interrupted {
            130
        } else {
            eprintln!("clud setup: {error}");
            1
        }
    })?
    .ok_or(130)
}

/// Interactive provider menu; the terminal must be a TTY.
pub fn choose_provider() -> Result<ModelProvider, i32> {
    Ok(OFFERED[choose("Setup for:", PROVIDER_LABELS)?])
}

fn other_model() -> Result<String, i32> {
    eprint!("OpenRouter model ID: ");
    io::stderr().flush().map_err(|_| 1)?;
    let mut model = String::new();
    if io::stdin().read_line(&mut model).map_err(|_| 1)? == 0 {
        return Err(130);
    }
    let model = model.trim();
    if model.is_empty() || model.chars().any(char::is_control) {
        eprintln!("clud setup: model ID cannot be empty or contain control characters");
        return Err(1);
    }
    Ok(model.to_owned())
}

/// Interactive OpenRouter model choice, saved as the OpenRouter default.
pub fn choose_openrouter_model() -> Result<String, i32> {
    let model = match choose("Select a model:", &[DEEPSEEK, GLM, "Other:"])? {
        0 => DEEPSEEK.to_owned(),
        1 => GLM.to_owned(),
        _ => other_model()?,
    };
    crate::clud_settings::save_openrouter_model(model.clone()).map_err(|error| {
        eprintln!("clud setup: could not save OpenRouter model: {error}");
        1
    })?;
    Ok(model)
}

/// Run the masked `clud auth login openrouter` prompt (existing store).
pub fn login_openrouter() -> Result<(), i32> {
    let descriptor = crate::provider_registry::descriptor_for(ModelProvider::OpenRouter)
        .expect("OpenRouter descriptor");
    match crate::provider_auth::run_for(descriptor, &DeepseekAuthSubcommand::Login) {
        0 => Ok(()),
        code => Err(code),
    }
}

/// Persist `provider` as the default with the native harness.
pub fn save_provider(home: &Path, provider: ModelProvider) -> Result<(), String> {
    crate::clud_settings::save_settings_patch_at(
        home,
        crate::clud_settings::GlobalSettingsPatch {
            model_provider: Some(provider),
            harness: Some(HarnessSelection::Default),
            provider_profiles: vec![crate::clud_settings::ProviderProfilePatch {
                provider: Some(provider),
                harness: Some(HarnessSelection::Default),
                ..crate::clud_settings::ProviderProfilePatch::default()
            }],
            ..crate::clud_settings::GlobalSettingsPatch::default()
        },
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_model_provider_is_offered_or_explicitly_excluded() {
        for &provider in ModelProvider::ALL {
            let offered = OFFERED.contains(&provider);
            let excluded = NOT_OFFERED.contains(&provider);
            assert!(
                offered ^ excluded,
                "{provider:?} must be in exactly one of OFFERED / NOT_OFFERED"
            );
        }
        assert_eq!(OFFERED.len(), PROVIDER_LABELS.len());
    }

    #[test]
    fn save_provider_round_trips_for_every_offered_provider() {
        for provider in OFFERED {
            let home = tempfile::tempdir().unwrap();
            save_provider(home.path(), provider).unwrap();
            let prefs =
                crate::clud_settings::load_global_launch_preferences_at(home.path()).unwrap();
            assert_eq!(prefs.model_provider, Some(provider));
            assert_eq!(prefs.harness, Some(HarnessSelection::Default));
        }
    }

    #[test]
    fn choices_default_to_first_and_support_cancel() {
        for (title, labels) in [
            ("Setup for:", PROVIDER_LABELS),
            ("Select a model:", &[DEEPSEEK, GLM, "Other:"][..]),
        ] {
            let mut choice = Choice {
                title,
                labels,
                selected: 0,
            };
            assert_eq!(choice.on_key(Key::Enter), Step::Done(Some(0)));
            assert_eq!(choice.on_key(Key::Down), Step::Redraw);
            assert_eq!(choice.on_key(Key::Enter), Step::Done(Some(1)));
            assert_eq!(choice.on_key(Key::Escape), Step::Done(None));
            assert_eq!(choice.view(Duration::ZERO).rows[1].marker, "[*]");
        }
    }
}

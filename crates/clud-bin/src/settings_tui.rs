//! `clud settings` — a small, cross-platform TUI menu over the typed settings
//! in `~/.clud/settings.json`.
//!
//! `Menu` is a pure, unit-tested state machine. All terminal I/O (raw mode,
//! keys, CRLF rendering, redraw) belongs to the shared [`crate::selector`]
//! (#1195), as it does for the launch-scope selector and the harness picker.
//! Provider and harness choice rows share `preference::ChoiceSelector` with
//! the inline launch-scope selector; all rows save in one atomic patch.

use std::io::{self, IsTerminal};
use std::time::Duration;

use crate::backend::{HarnessSelection, ModelProvider};
use crate::clud_settings;
use crate::preference::{ChoiceOption, ChoiceSelector};
use crate::provider_catalog::{self, EffortLevel};
use crate::selector::{self, Key, Note, Row, Selector, Step, View};

/// One selector row per [`ModelProvider::ALL`] entry, built at call time
/// (rather than a `const` array) so a new registered provider needs no
/// second array here. `label` intentionally reuses `as_str()` rather than a
/// display name: this is the lowercase `--claude`/`--codex`/`--deepseek`-
/// style value cycled through and written to `settings.json`, not prose.
fn model_options() -> Vec<ChoiceOption<ModelProvider>> {
    ModelProvider::ALL
        .iter()
        .map(|&value| ChoiceOption {
            value,
            label: value.as_str(),
            note: "",
        })
        .collect()
}

const HARNESS_OPTIONS: [ChoiceOption<HarnessSelection>; 5] = [
    ChoiceOption {
        value: HarnessSelection::Default,
        label: "default",
        note: "",
    },
    ChoiceOption {
        value: HarnessSelection::Claude,
        label: "claude",
        note: "",
    },
    ChoiceOption {
        value: HarnessSelection::Codex,
        label: "codex",
        note: "",
    },
    ChoiceOption {
        value: HarnessSelection::DeepSeek,
        label: "deepseek",
        note: "",
    },
    ChoiceOption {
        value: HarnessSelection::OpenCode,
        label: "opencode",
        note: "",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum SettingValue {
    Bool(bool),
    BlockCd(crate::repo_clud_config::BlockCd),
    ModelProvider(ModelProvider),
    Harness(HarnessSelection),
    Model {
        provider: ModelProvider,
        value: &'static str,
    },
    Effort {
        provider: ModelProvider,
        value: Option<EffortLevel>,
    },
    ContextWindow {
        provider: ModelProvider,
        value: Option<&'static str>,
    },
}

impl SettingValue {
    fn cycle(&mut self) {
        match self {
            Self::Bool(value) => *value = !*value,
            Self::BlockCd(value) => {
                use crate::repo_clud_config::BlockCd;
                *value = match value {
                    BlockCd::Auto => BlockCd::Always,
                    BlockCd::Always => BlockCd::Never,
                    BlockCd::Never => BlockCd::Auto,
                };
            }
            Self::ModelProvider(value) => {
                let mut selector = ChoiceSelector::new(&model_options(), *value, *value);
                selector.cycle();
                *value = selector.selected();
            }
            Self::Harness(value) => {
                let mut selector = ChoiceSelector::new(&HARNESS_OPTIONS, *value, *value);
                selector.cycle();
                *value = selector.selected();
            }
            Self::Model { provider, value } => {
                let options = provider_catalog::models_for_provider(*provider)
                    .map(|model| model.cli_id)
                    .collect::<Vec<_>>();
                let index = options
                    .iter()
                    .position(|candidate| candidate == value)
                    .unwrap_or(0);
                *value = options[(index + 1) % options.len()];
            }
            Self::Effort { provider, value } => {
                let options = provider_catalog::supported_efforts(*provider);
                *value = match value {
                    None => options.first().copied(),
                    Some(current) => options
                        .iter()
                        .position(|candidate| candidate == current)
                        .and_then(|index| options.get(index + 1).copied()),
                };
            }
            Self::ContextWindow { provider, value } => {
                let options = provider_catalog::supported_context_windows(*provider);
                *value = match value {
                    None => options.first().copied(),
                    Some(current) => options
                        .iter()
                        .position(|candidate| candidate == current)
                        .and_then(|index| options.get(index + 1).copied()),
                };
            }
        }
    }

    fn marker(&self) -> String {
        match self {
            Self::Bool(true) => "[x]".to_string(),
            Self::Bool(false) => "[ ]".to_string(),
            Self::BlockCd(value) => format!("[{}]", value.label()),
            Self::ModelProvider(value) => format!("[{}]", value.as_str()),
            Self::Harness(value) => format!("[{}]", value.as_str()),
            Self::Model { value, .. } => {
                let display_name = provider_catalog::model_by_cli_id(value)
                    .map_or(*value, |model| model.display_name);
                format!("[{display_name}]")
            }
            Self::Effort { value, .. } => {
                format!("[{}]", value.map(EffortLevel::as_str).unwrap_or("default"))
            }
            Self::ContextWindow { value, .. } => {
                format!("[{}]", value.unwrap_or("default"))
            }
        }
    }

    fn list_value(&self) -> String {
        match self {
            Self::Bool(true) => "true".to_string(),
            Self::Bool(false) => "false".to_string(),
            Self::BlockCd(value) => value.label().to_string(),
            Self::ModelProvider(value) => value.as_str().to_string(),
            Self::Harness(value) => value.as_str().to_string(),
            Self::Model { value, .. } => (*value).to_string(),
            Self::Effort { value, .. } => value
                .map(EffortLevel::as_str)
                .unwrap_or("default")
                .to_string(),
            Self::ContextWindow { value, .. } => value.unwrap_or("default").to_string(),
        }
    }
}

#[derive(Clone)]
struct SettingItem {
    key: &'static str,
    label: &'static str,
    note: &'static str,
    value: SettingValue,
}

fn setting_items() -> Vec<SettingItem> {
    let launch = clud_settings::load_global_launch_preferences().unwrap_or_default();
    let snapshot = clud_settings::load_launch_preferences_read_only().unwrap_or_default();
    let mut items = vec![
        SettingItem {
            key: "backend.default",
            label: "Default model provider",
            note: "Used when neither --claude, --codex, nor --deepseek is supplied.",
            value: SettingValue::ModelProvider(
                launch.model_provider.unwrap_or(ModelProvider::Claude),
            ),
        },
        SettingItem {
            key: "harness.default",
            label: "Agent harness",
            note: "default follows the model provider; explicit overrides are announced.",
            value: SettingValue::Harness(launch.harness.unwrap_or_default()),
        },
    ];
    for &provider in ModelProvider::ALL {
        append_provider_profile_items(&mut items, &snapshot, provider);
    }
    items.push(SettingItem {
        key: "git.pr_wait_fail_fast",
        label: "Fail-fast PR-check watches",
        note: "Routes session PR-check watches through the bundled fail-fast \
               waiter. PR polling loops stay blocked; run watches are not PR \
               waits and fall through (DD-189); without \
               the shim, native PR-check watches stay blocked. On by default (DD-116).",
        value: SettingValue::Bool(clud_settings::load_pr_wait_fail_fast_enabled().unwrap_or(true)),
    });
    items.push(SettingItem {
        key: "git.gh_read_broker",
        label: "Cache session gh api reads",
        note: "Answers session `gh api` GETs from the clud daemon's shared \
               ETag cache (30-60 s TTL, free 304 revalidation) to save the \
               GitHub REST budget. Output is byte-identical; any miss runs \
               the real gh. On by default (#1743).",
        value: SettingValue::Bool(clud_settings::load_gh_read_broker_enabled().unwrap_or(true)),
    });
    items.push(SettingItem {
        key: "bash.block_cd",
        label: "Pin the session cwd to the repo root",
        note: "A stray `cd` moves the cwd for every later tool call and breaks                repo-relative hooks. `auto` decides per repo from the hooks in                scope; a repo's .clud/settings.json overrides this.",
        value: SettingValue::BlockCd(clud_settings::load_block_cd().unwrap_or_default()),
    });
    items.push(SettingItem {
        key: "web_term.enabled",
        label: "Open bare clud launches in the web terminal",
        note: "Uses the bundled desktop companion; disabled by default.",
        value: SettingValue::Bool(clud_settings::load_web_term_enabled().unwrap_or(false)),
    });
    items.push(gc_delete_remote_branches_item(
        clud_settings::load_gc_delete_remote_branches().unwrap_or(false),
        std::env::var(GC_DELETE_REMOTE_BRANCHES_ENV).ok().as_deref(),
    ));
    items
}

/// The daemon GC reads this env var before `gc.delete_remote_branches`
/// (`daemon/gc_service.rs`), so the row says when the saved value is moot.
const GC_DELETE_REMOTE_BRANCHES_ENV: &str = "CLUD_GC_DELETE_REMOTE_BRANCHES";

const GC_DELETE_REMOTE_BRANCHES_NOTE: &str =
    "When the daemon reclaims a worktree whose PR was merged, also delete its \
     branch on origin. Leased on the verified tip \
     (--force-with-lease=<ref>:<tip>): a branch that moved is never deleted. \
     Off by default.";

/// #1608: the `gc.delete_remote_branches` row (#1603). `env_override` is the
/// raw `CLUD_GC_DELETE_REMOTE_BRANCHES` value, if set.
fn gc_delete_remote_branches_item(value: bool, env_override: Option<&str>) -> SettingItem {
    let note = match env_override {
        Some(_) => leak_string(format!(
            "{GC_DELETE_REMOTE_BRANCHES_NOTE} Currently overridden by \
             {GC_DELETE_REMOTE_BRANCHES_ENV}."
        )),
        None => GC_DELETE_REMOTE_BRANCHES_NOTE,
    };
    SettingItem {
        key: "gc.delete_remote_branches",
        label: "Delete merged branches on origin during GC",
        note,
        value: SettingValue::Bool(value),
    }
}

/// Human-readable provider name for TUI labels. Anthropic-compat providers
/// (DeepSeek, and Kimi once it lands) carry irregular capitalization
/// (`"DeepSeek"`, not `"Deepseek"`) recorded once in their registry
/// descriptor, so this defers to that when one exists; Claude and Codex have
/// no descriptor (they are native/translation-bridge, not vault providers)
/// and fall back to capitalizing `as_str()`, which is exact for both.
fn provider_display_name(provider: ModelProvider) -> String {
    if let Some(descriptor) = crate::provider_registry::descriptor_for(provider) {
        return descriptor.display_name.to_string();
    }
    let mut chars = provider.as_str().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Leaks a short-lived formatted string to `'static` so it can populate
/// `SettingItem::key`/`label` without changing that struct's field types.
/// `clud settings` builds this list at most once per process invocation, so
/// the leak is bounded and never accumulates across a long-running loop.
fn leak_string(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

fn append_provider_profile_items(
    items: &mut Vec<SettingItem>,
    snapshot: &clud_settings::LaunchPreferencesSnapshot,
    provider: ModelProvider,
) {
    let profile = snapshot.profile(provider);
    let fallback_model = provider_catalog::reviewed_default_model(provider)
        .or_else(|| provider_catalog::models_for_provider(provider).next())
        .expect("every provider has catalog models");
    let selected_model = profile
        .and_then(|profile| profile.model.as_deref())
        .and_then(provider_catalog::model_by_cli_id)
        .unwrap_or(fallback_model);
    let context_window = profile
        .and_then(|profile| profile.context_window.as_deref())
        .and_then(|value| match value {
            "auto" => Some("auto"),
            "1m" => Some("1m"),
            _ => None,
        });
    let settings_id = provider.as_str();
    let display = provider_display_name(provider);
    let model_key = leak_string(format!("providers.{settings_id}.model"));
    let harness_key = leak_string(format!("providers.{settings_id}.harness"));
    let effort_key = leak_string(format!("providers.{settings_id}.effort"));
    let context_key = leak_string(format!("providers.{settings_id}.context_window"));
    let model_label = leak_string(format!("{display} model"));
    let harness_label = leak_string(format!("{display} harness"));
    let effort_label = leak_string(format!("{display} effort"));
    let context_label = leak_string(format!("{display} context window"));
    items.extend([
        SettingItem {
            key: model_key,
            label: model_label,
            note: "Canonical catalog model for direct launches.",
            value: SettingValue::Model {
                provider,
                value: selected_model.cli_id,
            },
        },
        SettingItem {
            key: harness_key,
            label: harness_label,
            note: "Harness used when this provider is selected explicitly.",
            value: SettingValue::Harness(
                profile
                    .and_then(|profile| profile.harness)
                    .unwrap_or_default(),
            ),
        },
        SettingItem {
            key: effort_key,
            label: effort_label,
            note: "Provider-scoped launch effort; default uses catalog policy.",
            value: SettingValue::Effort {
                provider,
                value: profile.and_then(|profile| profile.effort),
            },
        },
        SettingItem {
            key: context_key,
            label: context_label,
            note: "Provider-scoped context; default uses catalog policy.",
            value: SettingValue::ContextWindow {
                provider,
                value: context_window,
            },
        },
    ]);
}

pub fn run(list_only: bool) -> i32 {
    let items = setting_items();

    if list_only {
        for item in &items {
            println!(
                "{} = {}  # {}",
                item.key,
                item.value.list_value(),
                item.note
            );
        }
        return 0;
    }

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!(
            "clud settings requires an interactive terminal. Use `clud settings --list` to view current values."
        );
        return 1;
    }

    match run_interactive(items) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("clud settings: {error}");
            1
        }
    }
}

/// How the menu closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuExit {
    Unchanged,
    Save,
    Discard,
}

struct Menu {
    items: Vec<SettingItem>,
    original: Vec<SettingValue>,
    cursor: usize,
    /// Quitting with unsaved changes asks `[Y/n]` under the rows.
    confirming: bool,
}

impl Menu {
    fn new(items: Vec<SettingItem>) -> Self {
        let original = items.iter().map(|item| item.value.clone()).collect();
        Self {
            items,
            original,
            cursor: 0,
            confirming: false,
        }
    }

    fn is_dirty(&self) -> bool {
        self.items
            .iter()
            .map(|item| &item.value)
            .ne(self.original.iter())
    }
}

impl Selector for Menu {
    type Outcome = MenuExit;

    fn view(&self, _elapsed: Duration) -> View {
        View {
            title: "clud settings".to_string(),
            hints: vec!["Space toggle, q quit".to_string()],
            gap: true,
            rows: self
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| Row {
                    current: index == self.cursor,
                    marker: item.value.marker(),
                    label: item.label.to_string(),
                    note: Note::Below(item.note.to_string()),
                })
                .collect(),
            footer: if self.confirming {
                vec!["Unsaved changes. Save before exiting? [Y/n]".to_string()]
            } else {
                Vec::new()
            },
        }
    }

    fn on_key(&mut self, key: Key) -> Step<MenuExit> {
        if self.confirming {
            return match key {
                Key::Enter | Key::Char('y' | 'Y') => Step::Done(MenuExit::Save),
                Key::Char('n' | 'N') => Step::Done(MenuExit::Discard),
                Key::Escape => {
                    self.confirming = false;
                    Step::Redraw
                }
                _ => Step::Stay,
            };
        }
        match key {
            Key::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                Step::Redraw
            }
            Key::Down => {
                if self.cursor + 1 < self.items.len() {
                    self.cursor += 1;
                }
                Step::Redraw
            }
            Key::Space => {
                if let Some(item) = self.items.get_mut(self.cursor) {
                    item.value.cycle();
                }
                Step::Redraw
            }
            Key::Char('q') if self.is_dirty() => {
                self.confirming = true;
                Step::Redraw
            }
            Key::Char('q') => Step::Done(MenuExit::Unchanged),
            _ => Step::Stay,
        }
    }
}

fn run_interactive(items: Vec<SettingItem>) -> io::Result<()> {
    let mut menu = Menu::new(items);
    if selector::run(&mut io::stdout(), &mut menu)? == MenuExit::Save {
        clud_settings::save_settings_patch(patch_from_menu(&menu))
            .map_err(|error| io::Error::other(format!("saving settings: {error}")))?;
    }
    Ok(())
}

fn patch_from_menu(menu: &Menu) -> clud_settings::GlobalSettingsPatch {
    let mut patch = clud_settings::GlobalSettingsPatch::default();
    for (item, original) in menu.items.iter().zip(&menu.original) {
        if &item.value == original {
            continue;
        }
        match (item.key, &item.value) {
            ("backend.default", SettingValue::ModelProvider(value)) => {
                patch.model_provider = Some(*value);
            }
            ("harness.default", SettingValue::Harness(value)) => {
                patch.harness = Some(*value);
            }
            (key, SettingValue::Model { provider, value }) if key.starts_with("providers.") => {
                provider_patch(&mut patch, *provider).model = Some((*value).to_string());
            }
            (key, SettingValue::Harness(value)) if key.starts_with("providers.") => {
                if let Some(provider) = provider_from_profile_key(key) {
                    provider_patch(&mut patch, provider).harness = Some(*value);
                }
            }
            (key, SettingValue::Effort { provider, value }) if key.starts_with("providers.") => {
                provider_patch(&mut patch, *provider).effort = Some(*value);
            }
            (key, SettingValue::ContextWindow { provider, value })
                if key.starts_with("providers.") =>
            {
                provider_patch(&mut patch, *provider).context_window =
                    Some(value.map(str::to_string));
            }
            ("git.pr_wait_fail_fast", SettingValue::Bool(value)) => {
                patch.pr_wait_fail_fast = Some(*value);
            }
            ("git.gh_read_broker", SettingValue::Bool(value)) => {
                patch.gh_read_broker = Some(*value);
            }
            ("bash.block_cd", SettingValue::BlockCd(value)) => {
                patch.block_cd = Some(*value);
            }
            ("web_term.enabled", SettingValue::Bool(value)) => {
                patch.web_term = Some(*value);
            }
            ("gc.delete_remote_branches", SettingValue::Bool(value)) => {
                patch.gc_delete_remote_branches = Some(*value);
            }
            _ => {}
        }
    }
    patch
}

fn provider_patch(
    patch: &mut clud_settings::GlobalSettingsPatch,
    provider: ModelProvider,
) -> &mut clud_settings::ProviderProfilePatch {
    if let Some(index) = patch
        .provider_profiles
        .iter()
        .position(|profile| profile.provider == Some(provider))
    {
        return &mut patch.provider_profiles[index];
    }
    patch
        .provider_profiles
        .push(clud_settings::ProviderProfilePatch {
            provider: Some(provider),
            ..clud_settings::ProviderProfilePatch::default()
        });
    patch.provider_profiles.last_mut().unwrap()
}

fn provider_from_profile_key(key: &str) -> Option<ModelProvider> {
    let provider = key.strip_prefix("providers.")?.split('.').next()?;
    ModelProvider::from_settings_str(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(value: bool) -> SettingItem {
        SettingItem {
            key: "test.key",
            label: "Test setting",
            note: "note",
            value: SettingValue::Bool(value),
        }
    }

    #[test]
    fn toggle_flips_value_and_marks_dirty() {
        let mut menu = Menu::new(vec![item(false)]);
        assert!(!menu.is_dirty());
        assert_eq!(menu.on_key(Key::Space), Step::Redraw);
        assert_eq!(menu.items[0].value, SettingValue::Bool(true));
        assert!(menu.is_dirty());
    }

    #[test]
    fn toggle_twice_returns_to_clean() {
        let mut menu = Menu::new(vec![item(false)]);
        menu.on_key(Key::Space);
        menu.on_key(Key::Space);
        assert_eq!(menu.items[0].value, SettingValue::Bool(false));
        assert!(!menu.is_dirty());
    }

    #[test]
    fn quit_with_no_changes_exits_clean() {
        let mut menu = Menu::new(vec![item(false)]);
        assert_eq!(menu.on_key(Key::Char('q')), Step::Done(MenuExit::Unchanged));
    }

    #[test]
    fn quit_with_changes_asks_to_save_and_escape_returns_to_the_menu() {
        let mut menu = Menu::new(vec![item(false)]);
        menu.on_key(Key::Space);
        assert_eq!(menu.on_key(Key::Char('q')), Step::Redraw);
        assert!(menu.confirming);
        assert!(menu.view(Duration::ZERO).footer[0].contains("[Y/n]"));
        assert_eq!(
            menu.on_key(Key::Space),
            Step::Stay,
            "no editing while asked"
        );
        assert_eq!(menu.on_key(Key::Escape), Step::Redraw);
        assert!(!menu.confirming);
        assert!(menu.view(Duration::ZERO).footer.is_empty());
    }

    #[test]
    fn save_prompt_keys_choose_save_or_discard() {
        for (key, outcome) in [
            (Key::Char('y'), MenuExit::Save),
            (Key::Char('Y'), MenuExit::Save),
            (Key::Enter, MenuExit::Save),
            (Key::Char('n'), MenuExit::Discard),
            (Key::Char('N'), MenuExit::Discard),
        ] {
            let mut menu = Menu::new(vec![item(false)]);
            menu.on_key(Key::Space);
            menu.on_key(Key::Char('q'));
            assert_eq!(menu.on_key(Key::Char('z')), Step::Stay);
            assert_eq!(menu.on_key(key), Step::Done(outcome), "{key:?}");
        }
    }

    #[test]
    fn cursor_clamps_at_list_ends() {
        let mut menu = Menu::new(vec![item(false), item(true)]);
        assert_eq!(menu.cursor, 0);
        menu.on_key(Key::Up);
        assert_eq!(menu.cursor, 0, "cannot move above the first row");
        menu.on_key(Key::Down);
        assert_eq!(menu.cursor, 1);
        menu.on_key(Key::Down);
        assert_eq!(menu.cursor, 1, "cannot move below the last row");
    }

    #[test]
    fn toggle_only_affects_the_highlighted_row() {
        let mut menu = Menu::new(vec![item(false), item(false)]);
        menu.on_key(Key::Down);
        menu.on_key(Key::Space);
        assert_eq!(menu.items[0].value, SettingValue::Bool(false));
        assert_eq!(menu.items[1].value, SettingValue::Bool(true));
    }

    #[test]
    fn typed_model_and_harness_choices_share_cycle_behavior() {
        let mut model = SettingValue::ModelProvider(ModelProvider::Claude);
        model.cycle();
        assert_eq!(model, SettingValue::ModelProvider(ModelProvider::Codex));
        model.cycle();
        assert_eq!(model, SettingValue::ModelProvider(ModelProvider::DeepSeek));
        model.cycle();
        assert_eq!(model, SettingValue::ModelProvider(ModelProvider::Kimi));
        model.cycle();
        assert_eq!(
            model,
            SettingValue::ModelProvider(ModelProvider::OpenRouter)
        );
        model.cycle();
        assert_eq!(model, SettingValue::ModelProvider(ModelProvider::Claude));

        let mut harness = SettingValue::Harness(HarnessSelection::Default);
        harness.cycle();
        assert_eq!(harness, SettingValue::Harness(HarnessSelection::Claude));
        harness.cycle();
        assert_eq!(harness, SettingValue::Harness(HarnessSelection::Codex));
        harness.cycle();
        assert_eq!(harness, SettingValue::Harness(HarnessSelection::DeepSeek));
        harness.cycle();
        assert_eq!(harness, SettingValue::Harness(HarnessSelection::OpenCode));
        harness.cycle();
        assert_eq!(harness, SettingValue::Harness(HarnessSelection::Default));
    }

    #[test]
    fn model_marker_shows_checkpoint_while_list_value_stays_stable() {
        let model = SettingValue::Model {
            provider: ModelProvider::DeepSeek,
            value: "deepseek-v4-pro",
        };
        assert_eq!(model.marker(), "[DeepSeek V4 Pro 0813]");
        assert_eq!(model.list_value(), "deepseek-v4-pro");
    }

    #[test]
    fn typed_items_build_one_atomic_settings_patch() {
        let mut menu = Menu::new(vec![
            SettingItem {
                key: "backend.default",
                label: "",
                note: "",
                value: SettingValue::ModelProvider(ModelProvider::Claude),
            },
            SettingItem {
                key: "harness.default",
                label: "",
                note: "",
                value: SettingValue::Harness(HarnessSelection::Default),
            },
            SettingItem {
                key: "git.pr_wait_fail_fast",
                label: "",
                note: "",
                value: SettingValue::Bool(false),
            },
        ]);
        menu.items[0].value = SettingValue::ModelProvider(ModelProvider::Codex);
        menu.items[1].value = SettingValue::Harness(HarnessSelection::Claude);
        menu.items[2].value = SettingValue::Bool(true);
        assert_eq!(
            patch_from_menu(&menu),
            clud_settings::GlobalSettingsPatch {
                openrouter_zdr: None,
                block_cd: None,
                model_provider: Some(ModelProvider::Codex),
                harness: Some(HarnessSelection::Claude),
                pr_wait_fail_fast: Some(true),
                gh_read_broker: None,
                web_term: None,
                gc_delete_remote_branches: None,
                provider_profiles: Vec::new(),
            }
        );
    }

    #[test]
    fn unrelated_edit_does_not_materialize_launch_preferences() {
        let mut menu = Menu::new(vec![
            SettingItem {
                key: "backend.default",
                label: "",
                note: "",
                value: SettingValue::ModelProvider(ModelProvider::Claude),
            },
            SettingItem {
                key: "harness.default",
                label: "",
                note: "",
                value: SettingValue::Harness(HarnessSelection::Default),
            },
            SettingItem {
                key: "git.pr_wait_fail_fast",
                label: "",
                note: "",
                value: SettingValue::Bool(false),
            },
        ]);
        menu.items[2].value = SettingValue::Bool(true);
        assert_eq!(
            patch_from_menu(&menu),
            clud_settings::GlobalSettingsPatch {
                openrouter_zdr: None,
                block_cd: None,
                model_provider: None,
                harness: None,
                pr_wait_fail_fast: Some(true),
                gh_read_broker: None,
                web_term: None,
                gc_delete_remote_branches: None,
                provider_profiles: Vec::new(),
            }
        );
    }

    /// #1195: the menu used to draw with `writeln!` under raw mode, so on
    /// Linux and macOS every row and note walked diagonally. Drives a full
    /// session: toggle, quit, back out of the save prompt, quit, save.
    #[test]
    fn a_full_session_renders_crlf_frames_and_redraws_exactly_what_was_drawn() {
        use crate::selector::testing::{assert_crlf_only, key, ScriptedTerminal};

        let mut menu = Menu::new(vec![item(false), item(true)]);
        let mut terminal = ScriptedTerminal::new(
            [
                key(Key::Space),
                key(Key::Char('q')),
                key(Key::Escape),
                key(Key::Char('q')),
                key(Key::Char('y')),
            ],
            Duration::ZERO,
            80,
        );
        let mut out = Vec::new();
        let outcome = selector::drive(&mut out, &mut menu, &mut terminal).unwrap();
        assert_eq!(outcome, MenuExit::Save);

        let text = String::from_utf8(out).unwrap();
        assert_crlf_only(&text);
        // Title, hint and gap plus two rows of two lines is seven rows; the
        // save prompt adds an eighth. Space and both q presses move back over
        // seven rows; Escape moves back over the eight with the prompt.
        assert_eq!(text.matches("\x1b[7A\x1b[J").count(), 3, "{text:?}");
        assert_eq!(text.matches("\x1b[8A\x1b[J").count(), 1, "{text:?}");
        assert!(text.contains("  Unsaved changes. Save before exiting? [Y/n]\r\n"));
        assert!(text.contains("> [x] Test setting\r\n      note\r\n"));
    }

    #[test]
    fn block_cd_cycles_through_its_three_states() {
        use crate::repo_clud_config::BlockCd;
        let mut value = SettingValue::BlockCd(BlockCd::Auto);
        value.cycle();
        assert_eq!(value, SettingValue::BlockCd(BlockCd::Always));
        value.cycle();
        assert_eq!(value, SettingValue::BlockCd(BlockCd::Never));
        value.cycle();
        assert_eq!(value, SettingValue::BlockCd(BlockCd::Auto));
        assert_eq!(value.marker(), "[auto]");
        assert_eq!(value.list_value(), "auto");
    }

    #[test]
    fn block_cd_row_lands_in_the_patch() {
        use crate::repo_clud_config::BlockCd;
        let mut menu = Menu::new(vec![SettingItem {
            key: "bash.block_cd",
            label: "Pin the session cwd to the repo root",
            note: "",
            value: SettingValue::BlockCd(BlockCd::Auto),
        }]);
        menu.items[0].value = SettingValue::BlockCd(BlockCd::Never);

        assert_eq!(patch_from_menu(&menu).block_cd, Some(BlockCd::Never));
    }

    /// #1608: `gc.delete_remote_branches` is a TUI toggle whose note states
    /// the lease and merged-only scope, and an env override is surfaced.
    #[test]
    fn gc_delete_remote_branches_row_carries_the_lease_note() {
        let row = gc_delete_remote_branches_item(false, None);
        assert_eq!(row.key, "gc.delete_remote_branches");
        assert_eq!(row.value, SettingValue::Bool(false));
        assert!(
            row.note.contains("--force-with-lease=<ref>:<tip>"),
            "{}",
            row.note
        );
        assert!(row.note.contains("merged"), "{}", row.note);
        assert!(!row.note.contains("overridden"), "{}", row.note);

        let overridden = gc_delete_remote_branches_item(false, Some("1"));
        assert!(
            overridden
                .note
                .contains("overridden by CLUD_GC_DELETE_REMOTE_BRANCHES"),
            "{}",
            overridden.note
        );
    }

    /// #1608: flipping the row persists exactly the value the daemon GC
    /// reads (`load_gc_delete_remote_branches_at`) and nothing else.
    #[test]
    fn gc_delete_remote_branches_toggle_persists_only_that_key() {
        let home = tempfile::tempdir().unwrap();
        let clud_dir = home.path().join(".clud");
        std::fs::create_dir_all(&clud_dir).unwrap();
        let settings = clud_dir.join("settings.json");
        std::fs::write(
            &settings,
            r#"{"gc":{"delete_remote_branches":false},"web_term":{"enabled":true},"git":{"pr_wait_fail_fast":false}}"#,
        )
        .unwrap();
        assert!(!clud_settings::load_gc_delete_remote_branches_at(home.path()).unwrap());

        let mut menu = Menu::new(vec![
            item(false),
            gc_delete_remote_branches_item(false, None),
        ]);
        menu.on_key(Key::Down);
        menu.on_key(Key::Space);
        let patch = patch_from_menu(&menu);
        assert_eq!(
            patch,
            clud_settings::GlobalSettingsPatch {
                gc_delete_remote_branches: Some(true),
                ..clud_settings::GlobalSettingsPatch::default()
            }
        );
        clud_settings::save_settings_patch_at(home.path(), patch).unwrap();

        assert!(clud_settings::load_gc_delete_remote_branches_at(home.path()).unwrap());
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(doc["web_term"]["enabled"], serde_json::json!(true));
        assert_eq!(doc["git"]["pr_wait_fail_fast"], serde_json::json!(false));
    }
}

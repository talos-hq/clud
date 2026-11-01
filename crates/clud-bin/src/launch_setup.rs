//! Launch-scope selection and per-backend persistent setup.
//!
//! Session-only launches are the default for automation and one-shot prompt
//! paths. Interactive TUI launches can opt into global setup before the backend
//! starts.

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use crate::args::Args;
use crate::backend::{Backend, HarnessSelection, ModelProvider};
use crate::preference::{ChoiceOption, ChoiceSelector};
use crate::selector::{self, check_marker, Key, Note, Row, Selector, Step, View};
use crate::{codex_hook_normalize, skills};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchSetupScope {
    SessionOnly,
    Global,
}

impl LaunchSetupScope {
    pub fn as_str(self) -> &'static str {
        match self {
            LaunchSetupScope::SessionOnly => "session-only",
            LaunchSetupScope::Global => "global",
        }
    }

    pub fn from_settings_str(value: &str) -> Option<Self> {
        match value {
            "session-only" | "session_only" => Some(LaunchSetupScope::SessionOnly),
            "global" => Some(LaunchSetupScope::Global),
            _ => None,
        }
    }
}

const SCOPE_OPTIONS: [ChoiceOption<LaunchSetupScope>; 2] = [
    ChoiceOption {
        value: LaunchSetupScope::SessionOnly,
        label: "Session only",
        note: "this launch",
    },
    ChoiceOption {
        value: LaunchSetupScope::Global,
        label: "Globally",
        note: "remember launch preferences",
    },
];

/// The launch-scope choice. Terminal I/O belongs to [`crate::selector`]
/// (#1195), which also owns the CRLF rendering #1063 first fixed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSelector {
    choice: ChoiceSelector<LaunchSetupScope>,
}

impl Default for ScopeSelector {
    fn default() -> Self {
        Self {
            choice: ChoiceSelector::new(
                &SCOPE_OPTIONS,
                LaunchSetupScope::SessionOnly,
                LaunchSetupScope::SessionOnly,
            ),
        }
    }
}

impl ScopeSelector {
    pub fn selected(&self) -> LaunchSetupScope {
        self.choice.selected()
    }
}

impl Selector for ScopeSelector {
    type Outcome = LaunchSetupScope;

    fn view(&self, _elapsed: Duration) -> View {
        View {
            title: "Launch setup scope".to_string(),
            hints: vec!["Up/Down move, Enter select, Esc session-only".to_string()],
            gap: false,
            rows: self
                .choice
                .options()
                .iter()
                .map(|option| {
                    let current = option.value == self.selected();
                    Row {
                        current,
                        marker: check_marker(current).to_string(),
                        label: option.label.to_string(),
                        note: Note::Inline(option.note.to_string()),
                    }
                })
                .collect(),
            footer: Vec::new(),
        }
    }

    fn on_key(&mut self, key: Key) -> Step<LaunchSetupScope> {
        match key {
            Key::Up => {
                self.choice.previous();
                Step::Redraw
            }
            Key::Down => {
                self.choice.next();
                Step::Redraw
            }
            Key::Enter => Step::Done(self.choice.confirm()),
            Key::Escape => Step::Done(self.choice.cancel()),
            Key::Space | Key::Char(_) => Step::Stay,
        }
    }
}

pub fn should_prompt_for_scope(args: &Args, interactive_terminal: bool) -> bool {
    interactive_terminal
        && (args.explicit_model_provider().is_some() || args.harness.is_some())
        && !args.dry_run
        && args.prompt.is_none()
        && args.message.is_none()
        && !args.continue_session
        && args.resume.is_none()
        && args.command.is_none()
}

pub fn scope_for_non_prompting_launch(
    args: &Args,
    interactive_terminal: bool,
) -> Option<LaunchSetupScope> {
    (!should_prompt_for_scope(args, interactive_terminal)).then_some(LaunchSetupScope::SessionOnly)
}

pub fn scope_for_configured_launch(
    args: &Args,
    interactive_terminal: bool,
    configured_scope: Option<LaunchSetupScope>,
) -> Option<LaunchSetupScope> {
    if !args.dry_run {
        if let Some(scope) = configured_scope {
            return Some(scope);
        }
    }
    scope_for_non_prompting_launch(args, interactive_terminal)
}

pub fn scope_for_launch_selection(
    args: &Args,
    interactive_terminal: bool,
    configured_scope: Option<LaunchSetupScope>,
    configured_default_provider: Option<ModelProvider>,
    selected_provider: ModelProvider,
    configured_default_harness: Option<HarnessSelection>,
    selected_harness: HarnessSelection,
) -> Option<LaunchSetupScope> {
    let explicit_provider_changed = args.explicit_model_provider().is_some()
        && configured_default_provider.is_some_and(|provider| provider != selected_provider);
    let explicit_harness_changed = args.harness.is_some()
        && configured_default_harness.unwrap_or_default() != selected_harness;
    if should_prompt_for_scope(args, interactive_terminal)
        && (explicit_provider_changed || explicit_harness_changed)
    {
        return None;
    }
    scope_for_configured_launch(args, interactive_terminal, configured_scope)
}

pub fn should_persist_prompted_default_backend(args: &Args, scope: LaunchSetupScope) -> bool {
    !args.dry_run
        && (args.explicit_model_provider().is_some() || args.harness.is_some())
        && matches!(scope, LaunchSetupScope::Global)
}

/// Ask for the launch-setup scope. Ctrl-C and Ctrl-D return an
/// [`io::ErrorKind::Interrupted`] error.
pub fn prompt_scope<W: Write>(out: &mut W) -> io::Result<LaunchSetupScope> {
    selector::run(out, &mut ScopeSelector::default())
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("could not resolve user home directory")]
    NoHomeDir,
    #[error("{0}")]
    Skills(skills::InstallError),
    #[error("{0}")]
    Io(io::Error),
}

impl From<skills::InstallError> for SetupError {
    fn from(error: skills::InstallError) -> Self {
        SetupError::Skills(error)
    }
}

impl From<io::Error> for SetupError {
    fn from(error: io::Error) -> Self {
        SetupError::Io(error)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SetupReport {
    pub ran: Vec<&'static str>,
}

pub trait HarnessSetupAction {
    fn name(&self) -> &'static str;
    fn backend(&self) -> Backend;
    fn supports(&self, scope: LaunchSetupScope) -> bool;
    fn run(&self, ctx: &mut SetupContext<'_>) -> Result<(), SetupError>;
}

pub struct SetupContext<'a> {
    pub home: &'a Path,
    pub verbose: bool,
    pub out: &'a mut dyn Write,
}

struct BundledSkillsAction {
    backend: Backend,
}

impl HarnessSetupAction for BundledSkillsAction {
    fn name(&self) -> &'static str {
        "bundled-skills"
    }

    fn backend(&self) -> Backend {
        self.backend
    }

    fn supports(&self, scope: LaunchSetupScope) -> bool {
        matches!(scope, LaunchSetupScope::Global)
    }

    fn run(&self, ctx: &mut SetupContext<'_>) -> Result<(), SetupError> {
        // Only a genuine refresh is announced. `installed` is silent (a first
        // install is not news) and `skipped_existing` is the steady state. If
        // this line shows up on a launch that changed nothing, the comparison
        // in `skills::install_to` is wrong — that was issue #844.
        if let Some((_, report)) = skills::ensure_installed_for_backend_at(ctx.home, self.backend)?
        {
            for name in &report.refreshed {
                let _ = writeln!(ctx.out, "\x1b[32m[clud] updated /{name}\x1b[0m");
            }
            for name in &report.user_owned_diverged {
                let _ = writeln!(
                    ctx.out,
                    "\x1b[33m[clud] /{name} is user-owned and differs from this clud's bundled \
                     copy, so it is not updated; restore the `managed-by: clud` marker or \
                     delete its SKILL.md to get updates\x1b[0m"
                );
            }
        }
        // The /grind agents and workflow ride along with the skills that
        // route to them; they are Claude-only file kinds.
        if self.backend == Backend::Claude {
            if let Some(report) = crate::claude_files::ensure_installed_at(ctx.home)? {
                for path in &report.refreshed {
                    let _ = writeln!(ctx.out, "\x1b[32m[clud] updated ~/.claude/{path}\x1b[0m");
                }
                for path in &report.purged {
                    let _ = writeln!(ctx.out, "\x1b[32m[clud] removed ~/.claude/{path}\x1b[0m");
                }
            }
        }
        Ok(())
    }
}

struct CodexHookNormalizeAction;

impl HarnessSetupAction for CodexHookNormalizeAction {
    fn name(&self) -> &'static str {
        "codex-hook-normalize"
    }

    fn backend(&self) -> Backend {
        Backend::Codex
    }

    fn supports(&self, scope: LaunchSetupScope) -> bool {
        matches!(scope, LaunchSetupScope::Global)
    }

    fn run(&self, ctx: &mut SetupContext<'_>) -> Result<(), SetupError> {
        let clud_dir = ctx.home.join(".clud");
        let hooks_path = ctx.home.join(".codex").join("hooks.json");
        if let Err(error) =
            codex_hook_normalize::run_at(&clud_dir, &hooks_path, ctx.out, ctx.verbose)
        {
            if ctx.verbose {
                let _ = writeln!(ctx.out, "[clud] codex hook normalize: {error}");
            }
        }
        Ok(())
    }
}

/// #1972: keep `clud-cmd-scan` installed (and trusted) as a global Codex
/// `PreToolUse` hook so bare `codex` sessions are guarded too.
struct CodexCmdScanInstallAction;

impl HarnessSetupAction for CodexCmdScanInstallAction {
    fn name(&self) -> &'static str {
        "codex-cmd-scan-install"
    }

    fn backend(&self) -> Backend {
        Backend::Codex
    }

    fn supports(&self, scope: LaunchSetupScope) -> bool {
        matches!(scope, LaunchSetupScope::Global)
    }

    fn run(&self, ctx: &mut SetupContext<'_>) -> Result<(), SetupError> {
        let clud_dir = ctx.home.join(".clud");
        let codex_dir = ctx.home.join(".codex");
        if let Err(error) =
            crate::codex_cmd_scan_install::run_at(&clud_dir, &codex_dir, ctx.out, ctx.verbose)
        {
            if ctx.verbose {
                let _ = writeln!(ctx.out, "[clud] codex cmd-scan hook: {error}");
            }
        }
        Ok(())
    }
}

pub fn setup_actions() -> Vec<Box<dyn HarnessSetupAction>> {
    // Note: bundled Python tools (~/.clud/tools/*) are refreshed by
    // foreground startup and daemon startup, not as part of this
    // launch-setup pipeline. `clud tool run` also self-heals inline so
    // first-run hooks bypass NotFound. The launch setup actions here are
    // limited to backend-specific skills and codex hook normalization.
    //
    // #847: there is exactly one bundled-skill installer. A second action
    // used to write the same `~/.claude/skills/` files from a separate
    // registry, so each pass classified the other's output as drift and
    // rewrote it — reporting `updated` on every launch and silently
    // reverting the newer copies. One writer, one source of truth.
    vec![
        Box::new(BundledSkillsAction {
            backend: Backend::Claude,
        }),
        Box::new(BundledSkillsAction {
            backend: Backend::Codex,
        }),
        Box::new(CodexHookNormalizeAction),
        Box::new(CodexCmdScanInstallAction),
    ]
}

pub fn run_setup(
    scope: LaunchSetupScope,
    backend: Backend,
    verbose: bool,
    out: &mut dyn Write,
) -> Result<SetupReport, SetupError> {
    let home = crate::home::user_home().ok_or(SetupError::NoHomeDir)?;
    run_setup_at(&home, scope, backend, verbose, out)
}

pub fn run_setup_at(
    home: &Path,
    scope: LaunchSetupScope,
    backend: Backend,
    verbose: bool,
    out: &mut dyn Write,
) -> Result<SetupReport, SetupError> {
    if matches!(scope, LaunchSetupScope::SessionOnly) {
        return Ok(SetupReport::default());
    }

    let mut report = SetupReport::default();
    let mut ctx = SetupContext { home, verbose, out };
    for action in setup_actions() {
        if action.backend() == backend && action.supports(scope) {
            action.run(&mut ctx)?;
            report.ran.push(action.name());
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Args;
    use std::fs;
    use tempfile::tempdir;

    fn parse(argv: &[&str]) -> Args {
        Args::parse_from_raw(argv.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn selector_defaults_to_session_only() {
        let selector = ScopeSelector::default();
        assert_eq!(selector.selected(), LaunchSetupScope::SessionOnly);
        assert_eq!(
            selector::render(&selector.view(Duration::ZERO), 0).text(),
            "Launch setup scope\r\n  Up/Down move, Enter select, Esc session-only\r\n> [x] Session only   this launch\r\n  [ ] Globally       remember launch preferences\r\n"
        );
    }

    /// Issue #1063: the selector renders under raw mode, where `OPOST` is off
    /// and a bare `\n` moves down without returning to column zero. The shared
    /// renderer owns that now (#1195); this pins the scope prompt's frame and
    /// the redraw that moves back over it.
    #[test]
    fn navigating_redraws_the_four_rows_and_enter_keeps_the_frame() {
        use crate::selector::testing::{assert_crlf_only, key, ScriptedTerminal};

        let mut selector = ScopeSelector::default();
        let mut terminal =
            ScriptedTerminal::new([key(Key::Down), key(Key::Enter)], Duration::ZERO, 80);
        let mut out = Vec::new();
        let scope = selector::drive(&mut out, &mut selector, &mut terminal).unwrap();
        assert_eq!(scope, LaunchSetupScope::Global);

        let text = String::from_utf8(out).unwrap();
        assert_crlf_only(&text);
        assert_eq!(text.matches("\x1b[4A\x1b[J").count(), 1, "{text:?}");
        assert!(text.contains("> [x] Globally       remember launch preferences\r\n"));
        assert!(text.ends_with("\r\n\r\n"), "the frame stays in scrollback");
    }

    #[test]
    fn selector_navigation_and_enter() {
        let mut selector = ScopeSelector::default();
        assert_eq!(selector.on_key(Key::Down), Step::Redraw);
        assert_eq!(selector.selected(), LaunchSetupScope::Global);
        assert_eq!(selector.on_key(Key::Up), Step::Redraw);
        assert_eq!(selector.selected(), LaunchSetupScope::SessionOnly);
        assert_eq!(
            selector.on_key(Key::Enter),
            Step::Done(LaunchSetupScope::SessionOnly)
        );
    }

    #[test]
    fn selector_escape_chooses_session_only() {
        let mut selector = ScopeSelector::default();
        assert_eq!(selector.on_key(Key::Down), Step::Redraw);
        assert_eq!(selector.selected(), LaunchSetupScope::Global);
        assert_eq!(
            selector.on_key(Key::Escape),
            Step::Done(LaunchSetupScope::SessionOnly)
        );
    }

    #[test]
    fn prompt_scope_only_for_interactive_bare_launches() {
        assert!(should_prompt_for_scope(&parse(&["clud", "--codex"]), true));
        assert!(should_prompt_for_scope(
            &parse(&["clud", "--provider", "codex"]),
            true
        ));
        assert!(should_prompt_for_scope(&parse(&["clud", "--claude"]), true));
        assert!(should_prompt_for_scope(
            &parse(&["clud", "--deepseek"]),
            true
        ));
        assert!(!should_prompt_for_scope(&parse(&["clud"]), true));
        assert!(!should_prompt_for_scope(
            &parse(&["clud", "--codex"]),
            false
        ));
        assert!(!should_prompt_for_scope(
            &parse(&["clud", "--codex", "--dry-run"]),
            true
        ));
        assert!(!should_prompt_for_scope(
            &parse(&["clud", "--codex", "-p", "hello"]),
            true
        ));
        assert!(!should_prompt_for_scope(
            &parse(&["clud", "--codex", "loop"]),
            true
        ));
    }

    #[test]
    fn non_prompting_launches_default_to_session_only() {
        let args = parse(&["clud", "--codex", "--dry-run"]);
        assert_eq!(
            scope_for_non_prompting_launch(&args, true),
            Some(LaunchSetupScope::SessionOnly)
        );
    }

    #[test]
    fn configured_global_scope_skips_prompt_for_bare_launches() {
        let args = parse(&["clud", "--codex"]);
        assert_eq!(
            scope_for_configured_launch(&args, true, Some(LaunchSetupScope::Global)),
            Some(LaunchSetupScope::Global)
        );
    }

    #[test]
    fn explicit_backend_that_differs_from_stored_default_prompts_again() {
        let args = parse(&["clud", "--codex"]);
        assert_eq!(
            scope_for_launch_selection(
                &args,
                true,
                Some(LaunchSetupScope::Global),
                Some(ModelProvider::Claude),
                ModelProvider::Codex,
                None,
                HarnessSelection::Default,
            ),
            None
        );
    }

    #[test]
    fn generic_provider_that_differs_from_stored_default_prompts_again() {
        let args = parse(&["clud", "--provider", "codex"]);
        assert_eq!(
            scope_for_launch_selection(
                &args,
                true,
                Some(LaunchSetupScope::Global),
                Some(ModelProvider::Claude),
                ModelProvider::Codex,
                None,
                HarnessSelection::Default,
            ),
            None
        );
        assert!(should_persist_prompted_default_backend(
            &args,
            LaunchSetupScope::Global
        ));
    }

    #[test]
    fn explicit_deepseek_that_differs_from_stored_default_prompts_again() {
        let args = parse(&["clud", "--deepseek"]);
        assert_eq!(
            scope_for_launch_selection(
                &args,
                true,
                Some(LaunchSetupScope::Global),
                Some(ModelProvider::Claude),
                ModelProvider::DeepSeek,
                None,
                HarnessSelection::Default,
            ),
            None
        );
        assert!(should_persist_prompted_default_backend(
            &args,
            LaunchSetupScope::Global
        ));
    }

    #[test]
    fn explicit_backend_matching_stored_default_uses_configured_scope() {
        let args = parse(&["clud", "--codex"]);
        assert_eq!(
            scope_for_launch_selection(
                &args,
                true,
                Some(LaunchSetupScope::Global),
                Some(ModelProvider::Codex),
                ModelProvider::Codex,
                None,
                HarnessSelection::Default,
            ),
            Some(LaunchSetupScope::Global)
        );
    }

    #[test]
    fn one_shot_launches_do_not_prompt_when_backend_differs_from_default() {
        let args = parse(&["clud", "--codex", "-p", "hello"]);
        assert_eq!(
            scope_for_launch_selection(
                &args,
                true,
                Some(LaunchSetupScope::Global),
                Some(ModelProvider::Claude),
                ModelProvider::Codex,
                None,
                HarnessSelection::Default,
            ),
            Some(LaunchSetupScope::Global)
        );
    }

    #[test]
    fn interactive_harness_override_prompts_for_session_or_global_scope() {
        let args = parse(&["clud", "--codex", "--harness", "claude"]);
        assert_eq!(
            scope_for_launch_selection(
                &args,
                true,
                Some(LaunchSetupScope::Global),
                Some(ModelProvider::Codex),
                ModelProvider::Codex,
                Some(HarnessSelection::Default),
                HarnessSelection::Claude,
            ),
            None
        );
    }

    #[test]
    fn configured_global_scope_applies_to_one_shot_launches() {
        let args = parse(&["clud", "--codex", "-p", "hello"]);
        assert_eq!(
            scope_for_configured_launch(&args, true, Some(LaunchSetupScope::Global)),
            Some(LaunchSetupScope::Global)
        );
    }

    #[test]
    fn dry_run_ignores_configured_global_scope() {
        let args = parse(&["clud", "--codex", "--dry-run"]);
        assert_eq!(
            scope_for_configured_launch(&args, true, Some(LaunchSetupScope::Global)),
            Some(LaunchSetupScope::SessionOnly)
        );
    }

    #[test]
    fn global_explicit_backend_selection_persists_default_backend() {
        let args = parse(&["clud", "--codex"]);
        assert!(should_persist_prompted_default_backend(
            &args,
            LaunchSetupScope::Global
        ));
        assert!(!should_persist_prompted_default_backend(
            &args,
            LaunchSetupScope::SessionOnly
        ));
        assert!(!should_persist_prompted_default_backend(
            &parse(&["clud"]),
            LaunchSetupScope::Global
        ));
        assert!(!should_persist_prompted_default_backend(
            &parse(&["clud", "--codex", "--dry-run"]),
            LaunchSetupScope::Global
        ));
    }

    #[test]
    fn session_only_setup_does_not_write_agent_home_files() {
        let home = tempdir().unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::create_dir_all(home.path().join(".codex")).unwrap();

        let mut out = Vec::new();
        let report = run_setup_at(
            home.path(),
            LaunchSetupScope::SessionOnly,
            Backend::Codex,
            false,
            &mut out,
        )
        .unwrap();

        assert!(report.ran.is_empty());
        assert!(!home.path().join(".agents").exists());
        assert!(!home.path().join(".claude/skills").exists()); // skill-source-lint: allow (asserts install state, not a writer)
        assert!(!home.path().join(".clud").exists());
    }

    #[test]
    fn codex_global_setup_is_selected_backend_only() {
        let home = tempdir().unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        fs::write(
            home.path().join(".codex/hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"timeout":5}]}]}}"#,
        )
        .unwrap();

        let mut out = Vec::new();
        let report = run_setup_at(
            home.path(),
            LaunchSetupScope::Global,
            Backend::Codex,
            false,
            &mut out,
        )
        .unwrap();

        assert_eq!(
            report.ran,
            vec![
                "bundled-skills",
                "codex-hook-normalize",
                "codex-cmd-scan-install"
            ]
        );
        let codex_skill = ".codex/skills/clud-issue/SKILL.md"; // skill-source-lint: allow (asserts install state, not a writer)
        assert!(home.path().join(codex_skill).exists());
        assert!(!home.path().join(".agents").exists());
        assert!(!home.path().join(".claude/skills").exists()); // skill-source-lint: allow (asserts install state, not a writer)
        let hooks = fs::read_to_string(home.path().join(".codex/hooks.json")).unwrap();
        assert!(hooks.contains(r#""timeout": 30"#), "{hooks}");
        assert!(hooks.contains(r#""command": "clud-cmd-scan""#), "{hooks}");
        assert!(home.path().join(".clud/settings.json").exists());
    }

    #[test]
    fn claude_global_setup_is_selected_backend_only() {
        let home = tempdir().unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        fs::write(
            home.path().join(".codex/hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"timeout":5}]}]}}"#,
        )
        .unwrap();

        let mut out = Vec::new();
        let report = run_setup_at(
            home.path(),
            LaunchSetupScope::Global,
            Backend::Claude,
            false,
            &mut out,
        )
        .unwrap();

        // #844/#847: one skill installer, so one action. `claude-drift-skills`
        // used to run here too, rewriting these same files from a second
        // registry on every launch.
        assert_eq!(report.ran, vec!["bundled-skills"]);
        let claude_skill = ".claude/skills/clud-issue/SKILL.md"; // skill-source-lint: allow (asserts install state, not a writer)
        assert!(home.path().join(claude_skill).exists());
        assert!(!home.path().join(".agents").exists());
        // Launch setup does not install bundled tools. Foreground startup,
        // daemon startup, and `clud tool run` own that path. `.clud/` is
        // created by the bundled-skills action for settings.json under codex
        // setup, but the claude path does not touch it, so it stays absent
        // here.
        assert!(!home.path().join(".clud").exists());
        let hooks = fs::read_to_string(home.path().join(".codex/hooks.json")).unwrap();
        assert!(hooks.contains(r#""timeout":5"#), "{hooks}");
    }
}

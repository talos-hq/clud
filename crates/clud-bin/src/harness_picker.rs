//! Installed-harness discovery and the bare-launch countdown picker.

use std::io::{self, Write};
use std::time::Duration;

use crate::args::Args;
use crate::backend::{Backend, ModelProvider, RoutingMode};
use crate::selector::{self, check_marker, Key, Note, OnExit, Row, Selector, Step, View};
use running_process::ReadStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthState {
    KnownYes,
    KnownNo,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub struct CredentialSnapshot {
    pub deepseek: AuthState,
    pub claude: AuthState,
    pub codex: AuthState,
}

/// Never displace explicit routing or a saved direct-provider preference.
/// Unknown external authentication is deliberately not treated as absent.
pub fn deepseek_only_fallback(
    args: &Args,
    saved_provider: Option<ModelProvider>,
    auth: CredentialSnapshot,
) -> bool {
    deepseek_fallback_candidate(args, saved_provider)
        && auth.deepseek == AuthState::KnownYes
        && auth.claude == AuthState::KnownNo
        && auth.codex == AuthState::KnownNo
}

pub fn deepseek_fallback_candidate(args: &Args, saved_provider: Option<ModelProvider>) -> bool {
    matches!(args.command, None | Some(crate::args::Command::Run))
        && args.routing_mode() == RoutingMode::Direct
        && !args.dry_run
        && args.explicit_model_provider().is_none()
        && args.harness.is_none()
        && args.model.is_none()
        && args.effort.is_none()
        && args.context_window.is_none()
        && args.prompt.is_none()
        && args.message.is_none()
        && !args.continue_session
        && args.resume.is_none()
        && !args.detach
        && !args.detachable
        && args.transcript.is_none()
        && !args.experimental_daemon_centralized
        && args.daemon_mode.is_none()
        && args.passthrough.is_empty()
        && saved_provider.is_none()
}

/// Inspect only supported status surfaces; never read another CLI's secret
/// files or print status output (which may contain account information).
pub fn credential_snapshot() -> CredentialSnapshot {
    let deepseek = crate::provider_registry::descriptor_for(ModelProvider::DeepSeek)
        .expect("DeepSeek descriptor is registered");
    let deepseek = match crate::provider_auth::has_well_formed_stored_key(deepseek) {
        Ok(true) => AuthState::KnownYes,
        Ok(false) => AuthState::KnownNo,
        Err(_) => AuthState::Unknown,
    };
    if deepseek != AuthState::KnownYes {
        return CredentialSnapshot {
            deepseek,
            claude: AuthState::Unknown,
            codex: AuthState::Unknown,
        };
    }
    let claude = if credential_env_present(&["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"])
        || credential_env_present(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ]) {
        AuthState::KnownYes
    } else if let Some(path) = crate::backend_bootstrap::locate_installed_backend(Backend::Claude) {
        match status_command(vec![
            path.to_string_lossy().into_owned(),
            "auth".into(),
            "status".into(),
            "--json".into(),
        ]) {
            Ok((0, output)) => parse_claude_auth_status(&output),
            Ok(_) => AuthState::Unknown,
            Err(_) => AuthState::Unknown,
        }
    } else {
        AuthState::KnownNo
    };
    let clud_codex = crate::home::user_home()
        .map(|home| crate::codex_auth::load_at(&home))
        .unwrap_or(Err("home directory unavailable".to_string()));
    let codex = if credential_env_present(&["OPENAI_API_KEY", "CODEX_ACCESS_TOKEN"])
        || matches!(&clud_codex, Ok(Some(_)))
    {
        AuthState::KnownYes
    } else if clud_codex.is_err() {
        AuthState::Unknown
    } else if let Some(path) = crate::backend_bootstrap::locate_installed_backend(Backend::Codex) {
        match status_command(vec![
            path.to_string_lossy().into_owned(),
            "login".into(),
            "status".into(),
        ]) {
            Ok((exit, output)) => parse_codex_auth_status(exit, &output),
            Err(_) => AuthState::Unknown,
        }
    } else {
        AuthState::KnownNo
    };
    CredentialSnapshot {
        deepseek,
        claude,
        codex,
    }
}

fn parse_claude_auth_status(output: &[u8]) -> AuthState {
    match serde_json::from_slice::<serde_json::Value>(output)
        .ok()
        .and_then(|status| status.get("loggedIn").and_then(serde_json::Value::as_bool))
    {
        Some(true) => AuthState::KnownYes,
        Some(false) => AuthState::KnownNo,
        None => AuthState::Unknown,
    }
}

fn parse_codex_auth_status(exit: i32, output: &[u8]) -> AuthState {
    if exit == 0 {
        AuthState::KnownYes
    } else if String::from_utf8_lossy(output).trim() == "Not logged in" {
        AuthState::KnownNo
    } else {
        AuthState::Unknown
    }
}

fn credential_env_present(names: &[&str]) -> bool {
    names.iter().any(|name| {
        std::env::var(name)
            .ok()
            .is_some_and(|value| !value.trim().is_empty() && value != "0" && value != "false")
    })
}

fn status_command(argv: Vec<String>) -> Result<(i32, Vec<u8>), ()> {
    let process = crate::subprocess::ManagedSubprocess::start_inheriting_env(
        argv,
        None,
        true,
        crate::win_creation_flags::invisible_helper_creationflags(),
    )
    .map_err(|_| ())?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut output = Vec::new();
    loop {
        if std::time::Instant::now() >= deadline {
            let _ = process.kill();
            return Err(());
        }
        match process.read_stdout(Some(Duration::from_millis(100))) {
            ReadStatus::Line(line) => {
                output.extend_from_slice(&line);
                if output.len() > 8192 {
                    let _ = process.kill();
                    return Err(());
                }
            }
            ReadStatus::Timeout => {
                let _ = process.poll();
            }
            ReadStatus::Eof => break,
        }
    }
    let exit = process
        .wait(Some(Duration::from_millis(100)))
        .map_err(|_| ())?;
    Ok((exit, output))
}

pub const DEFAULT_COUNTDOWN: Duration = Duration::from_secs(3);

pub fn should_select(args: &Args, stdin_is_terminal: bool, stderr_is_terminal: bool) -> bool {
    stdin_is_terminal
        && stderr_is_terminal
        && !args.dry_run
        && args.command.is_none()
        && args.explicit_model_provider().is_none()
        && args.harness.is_none()
        && args.model.is_none()
        && args.effort.is_none()
        && args.context_window.is_none()
        && args.prompt.is_none()
        && args.message.is_none()
        && !args.continue_session
        && args.resume.is_none()
        && !args.detach
        && !args.detachable
        && args.transcript.is_none()
        && !args.experimental_daemon_centralized
        && args.daemon_mode.is_none()
        && args.passthrough.is_empty()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionFlow {
    NoneInstalled,
    Immediate(Backend),
    Prompt(Backend),
}

pub fn default_harness(installed: &[Backend], saved: Option<Backend>) -> Backend {
    saved
        .filter(|saved| installed.contains(saved))
        .unwrap_or(installed[0])
}

pub fn selection_flow(installed: &[Backend], saved: Option<Backend>) -> SelectionFlow {
    match installed {
        [] => SelectionFlow::NoneInstalled,
        [only] => SelectionFlow::Immediate(*only),
        many => SelectionFlow::Prompt(default_harness(many, saved)),
    }
}

pub fn discover_installed_with<F>(mut locate: F) -> Vec<Backend>
where
    F: FnMut(Backend) -> bool,
{
    Backend::ALL
        .into_iter()
        .filter(|backend| locate(*backend))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerOutcome {
    Selected(Backend),
    Cancelled,
}

/// How often the countdown wakes to redraw its seconds.
const COUNTDOWN_TICK: Duration = Duration::from_millis(100);

/// The picker's state. Terminal I/O belongs to [`crate::selector`] (#1195).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerModel {
    options: Vec<Backend>,
    selected: usize,
    countdown: Duration,
    countdown_active: bool,
    /// The seconds value last drawn, so a tick redraws only on a change.
    shown_seconds: u64,
}

impl PickerModel {
    pub fn new(options: Vec<Backend>, default: Backend, countdown: Duration) -> Self {
        assert!(options.len() >= 2, "picker requires at least two harnesses");
        let selected = options
            .iter()
            .position(|backend| *backend == default)
            .unwrap_or(0);
        let mut picker = Self {
            options,
            selected,
            countdown,
            countdown_active: true,
            shown_seconds: 0,
        };
        picker.shown_seconds = picker.remaining_seconds(Duration::ZERO);
        picker
    }

    pub fn selected(&self) -> Backend {
        self.options[self.selected]
    }

    pub fn countdown_active(&self) -> bool {
        self.countdown_active
    }

    pub fn remaining_seconds(&self, elapsed: Duration) -> u64 {
        if !self.countdown_active {
            return 0;
        }
        let remaining = self.countdown.saturating_sub(elapsed);
        remaining.as_millis().div_ceil(1_000) as u64
    }

    pub fn tick(&self, elapsed: Duration) -> Option<PickerOutcome> {
        (self.countdown_active && elapsed >= self.countdown)
            .then(|| PickerOutcome::Selected(self.selected()))
    }
}

impl Selector for PickerModel {
    type Outcome = PickerOutcome;

    fn view(&self, elapsed: Duration) -> View {
        let hint = if self.countdown_active {
            format!(
                "Auto-launching in {}s  |  Up/Down choose, Enter launch",
                self.remaining_seconds(elapsed)
            )
        } else {
            "Up/Down choose, Enter launch, Esc cancel".to_string()
        };
        View {
            title: "Select an agent harness".to_string(),
            hints: vec![hint],
            gap: true,
            rows: self
                .options
                .iter()
                .map(|backend| {
                    let current = *backend == self.selected();
                    Row {
                        current,
                        marker: check_marker(current).to_string(),
                        label: display_name(*backend).to_string(),
                        note: Note::None,
                    }
                })
                .collect(),
            footer: vec!["Last choice is remembered".to_string()],
        }
    }

    fn on_key(&mut self, key: Key) -> Step<PickerOutcome> {
        match key {
            Key::Up => {
                self.countdown_active = false;
                self.selected = self.selected.saturating_sub(1);
                Step::Redraw
            }
            Key::Down => {
                self.countdown_active = false;
                if self.selected + 1 < self.options.len() {
                    self.selected += 1;
                }
                Step::Redraw
            }
            Key::Enter => Step::Done(PickerOutcome::Selected(self.selected())),
            Key::Escape => Step::Done(PickerOutcome::Cancelled),
            Key::Space | Key::Char(_) => Step::Stay,
        }
    }

    fn tick_interval(&self) -> Option<Duration> {
        self.countdown_active.then_some(COUNTDOWN_TICK)
    }

    fn on_tick(&mut self, elapsed: Duration) -> Step<PickerOutcome> {
        if let Some(outcome) = self.tick(elapsed) {
            return Step::Done(outcome);
        }
        let remaining = self.remaining_seconds(elapsed);
        if self.countdown_active && remaining != self.shown_seconds {
            self.shown_seconds = remaining;
            Step::Redraw
        } else {
            Step::Stay
        }
    }

    fn on_exit(&self) -> OnExit {
        OnExit::Erase
    }
}

pub fn display_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Claude => "Claude Code",
        Backend::Codex => "Codex CLI",
        Backend::DeepSeek => "DeepSeek Harness",
        Backend::OpenCode => "OpenCode",
    }
}

/// Show the picker until a harness is chosen, the countdown confirms the
/// highlighted one, or the user cancels with Esc, Ctrl-C or Ctrl-D.
pub fn prompt<W: Write>(
    out: &mut W,
    options: Vec<Backend>,
    default: Backend,
) -> io::Result<PickerOutcome> {
    let mut picker = PickerModel::new(options, default, DEFAULT_COUNTDOWN);
    match selector::run(out, &mut picker) {
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(PickerOutcome::Cancelled),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use std::time::Duration;

    #[test]
    fn deepseek_sole_authorized_route_overrides_installed_harness_picker() {
        let args = parse(&["clud"]);
        let auth = CredentialSnapshot {
            deepseek: AuthState::KnownYes,
            claude: AuthState::KnownNo,
            codex: AuthState::KnownNo,
        };
        assert!(deepseek_only_fallback(&args, None, auth));
        assert!(!deepseek_only_fallback(
            &args,
            Some(crate::backend::ModelProvider::Claude),
            auth
        ));
        assert!(!deepseek_only_fallback(
            &parse(&["clud", "--claude"]),
            None,
            auth
        ));
        assert!(!deepseek_only_fallback(
            &parse(&["clud", "--dry-run"]),
            None,
            auth
        ));
        assert!(!deepseek_only_fallback(
            &parse(&["clud", "--harness", "deepseek"]),
            None,
            auth
        ));
        assert!(!deepseek_only_fallback(
            &parse(&["clud", "--unified"]),
            None,
            auth
        ));
        assert!(deepseek_only_fallback(&parse(&["clud", "run"]), None, auth));
        let target = crate::backend::resolve_routed_launch_target(
            RoutingMode::Direct,
            Some(ModelProvider::DeepSeek),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(target.model_provider, ModelProvider::DeepSeek);
        assert_eq!(target.effective_harness, Backend::Claude);
    }

    #[test]
    fn deepseek_fallback_requires_positive_exclusive_evidence() {
        let args = parse(&["clud"]);
        for state in [AuthState::KnownNo, AuthState::Unknown] {
            assert!(!deepseek_only_fallback(
                &args,
                None,
                CredentialSnapshot {
                    deepseek: state,
                    claude: AuthState::KnownNo,
                    codex: AuthState::KnownNo,
                }
            ));
        }
        for state in [AuthState::KnownYes, AuthState::Unknown] {
            assert!(!deepseek_only_fallback(
                &args,
                None,
                CredentialSnapshot {
                    deepseek: AuthState::KnownYes,
                    claude: state,
                    codex: AuthState::KnownNo,
                }
            ));
            assert!(!deepseek_only_fallback(
                &args,
                None,
                CredentialSnapshot {
                    deepseek: AuthState::KnownYes,
                    claude: AuthState::KnownNo,
                    codex: state,
                }
            ));
        }
    }

    #[test]
    fn claude_auth_status_requires_documented_boolean_field() {
        assert_eq!(
            parse_claude_auth_status(br#"{"loggedIn":true}"#),
            AuthState::KnownYes
        );
        assert_eq!(
            parse_claude_auth_status(br#"{"loggedIn":false}"#),
            AuthState::KnownNo
        );
        for output in [br#"{}"#.as_slice(), br#"{"loggedIn":"false"}"#, b"not JSON"] {
            assert_eq!(parse_claude_auth_status(output), AuthState::Unknown);
        }
    }

    #[test]
    fn codex_status_error_is_not_mistaken_for_logged_out() {
        assert_eq!(
            parse_codex_auth_status(1, b"Not logged in\n"),
            AuthState::KnownNo
        );
        assert_eq!(
            parse_codex_auth_status(1, b"Error checking login status"),
            AuthState::Unknown
        );
        assert_eq!(
            parse_codex_auth_status(0, b"Logged in using ChatGPT"),
            AuthState::KnownYes
        );
    }

    fn parse(argv: &[&str]) -> crate::args::Args {
        crate::args::Args::parse_from_raw(argv.iter().map(|arg| (*arg).to_string()).collect())
    }

    #[test]
    fn discovery_is_stably_ordered() {
        let installed = discover_installed_with(|backend| backend != Backend::Codex);
        assert_eq!(installed, vec![Backend::Claude, Backend::DeepSeek]);
    }

    #[test]
    fn only_bare_interactive_launches_offer_the_picker() {
        assert!(should_select(&parse(&["clud"]), true, true));
        for argv in [
            vec!["clud", "--codex"],
            vec!["clud", "--harness", "deepseek"],
            vec!["clud", "--dry-run"],
            vec!["clud", "-p", "hello"],
            vec!["clud", "--transcript", "session.log"],
            vec!["clud", "--experimental-daemon-centralized"],
            vec!["clud", "loop", "task"],
        ] {
            assert!(!should_select(&parse(&argv), true, true), "argv={argv:?}");
        }
        assert!(!should_select(&parse(&["clud"]), false, true));
        assert!(!should_select(&parse(&["clud"]), true, false));
    }

    #[test]
    fn stable_order_and_saved_default_fallback() {
        let installed = vec![Backend::Claude, Backend::Codex, Backend::DeepSeek];
        assert_eq!(
            default_harness(&installed, Some(Backend::Codex)),
            Backend::Codex
        );
        assert_eq!(default_harness(&installed, None), Backend::Claude);
        assert_eq!(
            default_harness(&[Backend::Codex, Backend::DeepSeek], Some(Backend::Claude)),
            Backend::Codex
        );
    }

    #[test]
    fn zero_one_and_many_installed_harnesses_choose_the_right_flow() {
        assert_eq!(selection_flow(&[], None), SelectionFlow::NoneInstalled);
        assert_eq!(
            selection_flow(&[Backend::Codex], Some(Backend::Claude)),
            SelectionFlow::Immediate(Backend::Codex)
        );
        assert_eq!(
            selection_flow(&[Backend::Claude, Backend::Codex], Some(Backend::Codex)),
            SelectionFlow::Prompt(Backend::Codex)
        );
    }

    #[test]
    fn timeout_confirms_default_without_input() {
        let picker = PickerModel::new(
            vec![Backend::Claude, Backend::Codex],
            Backend::Codex,
            Duration::from_secs(3),
        );
        assert_eq!(picker.tick(Duration::from_millis(2_999)), None);
        assert_eq!(
            picker.tick(Duration::from_secs(3)),
            Some(PickerOutcome::Selected(Backend::Codex))
        );
    }

    #[test]
    fn navigation_changes_selection_and_cancels_countdown() {
        let mut picker = PickerModel::new(
            vec![Backend::Claude, Backend::Codex, Backend::DeepSeek],
            Backend::Claude,
            Duration::from_secs(3),
        );
        assert_eq!(picker.on_key(Key::Down), Step::Redraw);
        assert_eq!(picker.selected(), Backend::Codex);
        assert!(!picker.countdown_active());
        assert_eq!(picker.tick_interval(), None, "no countdown, no ticks");
        assert_eq!(picker.tick(Duration::from_secs(30)), None);
        assert_eq!(
            picker.on_key(Key::Enter),
            Step::Done(PickerOutcome::Selected(Backend::Codex))
        );
    }

    #[test]
    fn renderer_marks_the_saved_default_and_shows_the_countdown() {
        let picker = PickerModel::new(
            vec![Backend::Claude, Backend::Codex, Backend::DeepSeek],
            Backend::Codex,
            Duration::from_secs(3),
        );
        let output = selector::render(&picker.view(Duration::ZERO), 0).text();

        assert!(output.contains("Select an agent harness"));
        assert!(output.contains("Auto-launching in 3s"));
        assert!(output.contains("> [x] Codex CLI"));
        assert!(output.contains("  [ ] Claude Code"));
        assert!(output.contains("  [ ] DeepSeek Harness"));
        selector::testing::assert_crlf_only(&output);
    }

    /// #1195: the picker used to draw with `writeln!` under raw mode, so each
    /// row started where the previous one ended on Linux and macOS.
    #[test]
    fn countdown_redraws_each_second_then_erases_and_auto_selects() {
        use selector::testing::ScriptedTerminal;

        let mut picker = PickerModel::new(
            vec![Backend::Claude, Backend::Codex],
            Backend::Codex,
            Duration::from_secs(3),
        );
        let mut terminal = ScriptedTerminal::new(std::iter::repeat_n(None, 30), COUNTDOWN_TICK, 80);
        let mut out = Vec::new();
        let outcome = selector::drive(&mut out, &mut picker, &mut terminal).unwrap();
        assert_eq!(outcome, PickerOutcome::Selected(Backend::Codex));

        let text = String::from_utf8(out).unwrap();
        for seconds in ["3s", "2s", "1s"] {
            assert!(
                text.contains(&format!("Auto-launching in {seconds}")),
                "{text:?}"
            );
        }
        // Title, hint, gap, two rows and the footer are six rows: two redraws
        // at the second boundaries and the final erase each move back six.
        assert_eq!(text.matches("\x1b[6A\x1b[J").count(), 3, "{text:?}");
        assert!(text.ends_with("\x1b[6A\x1b[J"), "the picker erases itself");
        selector::testing::assert_crlf_only(&text);
    }

    #[test]
    fn cancel_never_selects() {
        let mut picker = PickerModel::new(
            vec![Backend::Claude, Backend::Codex],
            Backend::Claude,
            Duration::from_secs(3),
        );
        assert_eq!(
            picker.on_key(Key::Escape),
            Step::Done(PickerOutcome::Cancelled)
        );
    }
}

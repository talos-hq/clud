use std::cell::Cell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use super::*;
use crate::self_install::activation::PathChange;

/// Presses the queued labels in order; records every dialog and script.
#[derive(Default)]
struct Scripted {
    presses: VecDeque<Option<&'static str>>,
    shown: Vec<(String, Vec<String>)>,
    terminals: Vec<String>,
    terminal_error: Option<String>,
}

impl Scripted {
    fn pressing(presses: &[Option<&'static str>]) -> Self {
        Self {
            presses: presses.iter().copied().collect(),
            ..Self::default()
        }
    }
}

impl Dialogs for Scripted {
    fn choose(&mut self, message: &str, buttons: &[&str]) -> Option<String> {
        self.shown.push((
            message.to_owned(),
            buttons.iter().map(|label| (*label).to_owned()).collect(),
        ));
        let press = self.presses.pop_front().expect("unexpected extra dialog")?;
        assert!(
            buttons.contains(&press),
            "{press:?} is not one of {buttons:?}"
        );
        Some(press.to_owned())
    }

    fn open_terminal(&mut self, script: &str) -> Result<(), String> {
        self.terminals.push(script.to_owned());
        self.terminal_error.clone().map_or(Ok(()), Err)
    }
}

fn facts(existing: Option<&str>) -> Facts {
    Facts {
        version: "2.8.70".into(),
        destination: "/Users/me/.local/bin/clud".into(),
        existing_version: existing.map(str::to_owned),
        replaces: existing.is_some(),
        path: Some(PathChange::Add {
            directory: "/Users/me/.local/bin".into(),
            files: vec!["/Users/me/.zprofile".into()],
        }),
        source: "running executable".into(),
        sha256: "ab".repeat(32),
        path_edit: "append to ~/.zprofile".into(),
        prompt_engine: "lokr".into(),
    }
}

fn exe() -> PathBuf {
    PathBuf::from("/Volumes/clud 2.8.70/Install clud.app/Contents/MacOS/clud")
}

fn never() -> Outcome {
    panic!("installed without consent")
}

#[test]
fn bundle_launch_is_recognized_only_from_contents_macos_with_no_arguments() {
    let exe = exe();
    assert_eq!(
        bundle_of(&exe),
        Some(PathBuf::from("/Volumes/clud 2.8.70/Install clud.app"))
    );
    let argv = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
    assert!(is_app_launch(&argv(&["clud"]), &exe));
    assert!(is_app_launch(&argv(&["clud", "-psn_0_12345"]), &exe));
    assert!(!is_app_launch(&argv(&["clud", "--installer"]), &exe));
    for other in [
        "/Users/me/.local/bin/clud",
        "/Applications/Install clud/Contents/MacOS/clud",
        "/x/Install clud.app/Contents/Resources/clud",
        "/x/Install clud.app/MacOS/clud",
    ] {
        assert!(
            !is_app_launch(&argv(&["clud"]), Path::new(other)),
            "{other}"
        );
    }
}

#[test]
fn the_installed_copy_is_the_bundle_helper_not_the_bound_main_executable() {
    assert_eq!(
        helper_of(&bundle_of(&exe()).unwrap()),
        PathBuf::from("/Volumes/clud 2.8.70/Install clud.app/Contents/Helpers/clud")
    );
    // The helper's own launch is never mistaken for a Finder start.
    let helper = helper_of(&bundle_of(&exe()).unwrap());
    assert!(!is_app_launch(&["clud".to_owned()], &helper));
}

#[test]
fn not_now_and_dismissal_write_nothing() {
    for press in [Some(NOT_NOW), None] {
        let mut dialogs = Scripted::pressing(&[press]);
        assert_eq!(flow(&mut dialogs, Ok(facts(None)), &exe(), never), 0);
        assert!(dialogs.terminals.is_empty());
        let (message, buttons) = &dialogs.shown[0];
        assert!(message.starts_with("Install clud 2.8.70"), "{message}");
        // `~` follows the real HOME, so only the directory tail is fixed.
        assert!(message.contains("Adds to PATH: "), "{message}");
        assert!(message.contains(".local/bin"), "{message}");
        assert_eq!(buttons, &[NOT_NOW, SHOW_DETAILS, "Install"]);
    }
}

#[test]
fn details_return_to_the_consent_dialog_then_a_complete_install_finishes() {
    let installs = Cell::new(0);
    let mut dialogs = Scripted::pressing(&[
        Some(SHOW_DETAILS),
        Some(BACK),
        Some("Install"),
        Some(OPEN_TERMINAL),
    ]);
    let code = flow(&mut dialogs, Ok(facts(None)), &exe(), || {
        installs.set(installs.get() + 1);
        Outcome::complete()
    });
    assert_eq!(code, 0);
    assert_eq!(installs.get(), 1);
    assert!(dialogs.shown[1].0.contains(&"ab".repeat(32)));
    assert!(dialogs.shown[3]
        .0
        .contains("Open a new terminal and type clud"));
    assert_eq!(dialogs.terminals.len(), 1);
    assert!(dialogs.terminals[0].contains("exec \"${SHELL:-/bin/zsh}\" -l"));
}

#[test]
fn an_existing_install_is_offered_as_update_or_reinstall() {
    for (existing, label) in [("2.8.65", "Update"), ("2.8.70", "Reinstall")] {
        let mut dialogs = Scripted::pressing(&[Some(label), Some(DONE)]);
        assert_eq!(
            flow(
                &mut dialogs,
                Ok(facts(Some(existing))),
                &exe(),
                Outcome::complete
            ),
            0
        );
        assert_eq!(dialogs.shown[0].1, [NOT_NOW, SHOW_DETAILS, label]);
        assert!(dialogs.terminals.is_empty());
    }
}

#[test]
fn every_incomplete_outcome_hands_off_to_the_interactive_installer() {
    let outcomes: [fn() -> Outcome; 3] = [
        Outcome::activation_required,
        || Outcome::failed(1, "installation_failed"),
        || Outcome::refused(2, "consent_required"),
    ];
    for outcome in outcomes {
        let mut dialogs = Scripted::pressing(&[Some("Install"), Some(FIX_IN_TERMINAL)]);
        let code = flow(&mut dialogs, Ok(facts(None)), &exe(), outcome);
        assert!(code >= 1, "an incomplete install must not exit 0");
        let (message, buttons) = &dialogs.shown[1];
        assert!(message.contains("was not fully installed"), "{message}");
        assert_eq!(buttons, &[CLOSE, FIX_IN_TERMINAL]);
        assert_eq!(
            dialogs.terminals,
            [
                "#!/bin/sh\nexec '/Volumes/clud 2.8.70/Install clud.app/Contents/MacOS/clud' \
              --installer\n"
            ]
        );
    }
}

#[test]
fn closing_the_rescue_dialog_opens_nothing_and_still_reports_failure() {
    let mut dialogs = Scripted::pressing(&[Some("Install"), Some(CLOSE)]);
    let code = flow(&mut dialogs, Ok(facts(None)), &exe(), || {
        Outcome::failed(1, "installation_failed")
    });
    assert_eq!(code, 1);
    assert!(dialogs.terminals.is_empty());
}

#[test]
fn a_planning_error_goes_straight_to_the_rescue_dialog() {
    let mut dialogs = Scripted::pressing(&[Some(FIX_IN_TERMINAL)]);
    let code = flow(&mut dialogs, Err("HOME is not set".into()), &exe(), never);
    assert_eq!(code, 1);
    assert!(dialogs.shown[0].0.contains("HOME is not set"));
    assert_eq!(dialogs.terminals.len(), 1);
}

#[test]
fn an_unopenable_terminal_prints_the_command_to_run_by_hand() {
    let mut dialogs = Scripted::pressing(&[Some(FIX_IN_TERMINAL), Some(CLOSE)]);
    dialogs.terminal_error = Some("no Terminal.app".into());
    flow(&mut dialogs, Err("broken".into()), &exe(), never);
    let (message, _) = &dialogs.shown[1];
    assert!(message.contains("no Terminal.app"));
    assert!(
        message.contains("'/Volumes/clud 2.8.70/Install clud.app/Contents/MacOS/clud' --installer")
    );
}

#[test]
fn shell_quote_survives_single_quotes() {
    assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
}

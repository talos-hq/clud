//! `Install clud.app`: the double-click installer inside the macOS disk image
//! (#2863, [native-installer.md](../../../../docs/architecture/native-installer.md#macos-disk-image-and-install-cludapp)).
//!
//! The bundle's main executable is the universal `clud` itself. Finder starts
//! it with no arguments and no terminal, so [`run`] recognizes that launch
//! from the executable's place in a `.app` bundle and drives the same plan
//! and transaction as `clud --installer --install-current` through native
//! dialogs instead of the terminal selector.
//!
//! The rule for everything that is not a clean install: never fail silently
//! and never retry on its own. Any outcome other than `complete` offers
//! **Fix in Terminal**, which opens Terminal on the interactive installer
//! (`clud --installer`), whose repair prompt already defaults to **Not now**.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::args::Args;

use super::catalog::compare_versions;
use super::result::Outcome;
use super::summary::{plain_text, Facts, PathStyle};

pub const NOT_NOW: &str = "Not now";
pub const SHOW_DETAILS: &str = "Show details";
pub const BACK: &str = "Back";
pub const DONE: &str = "Done";
pub const OPEN_TERMINAL: &str = "Open Terminal";
pub const CLOSE: &str = "Close";
pub const FIX_IN_TERMINAL: &str = "Fix in Terminal";

/// The native surface the bundle talks through; faked in tests.
pub trait Dialogs {
    /// Show `message` with `buttons` (the last is the default); the label
    /// pressed, or `None` when the dialog was dismissed or could not show.
    fn choose(&mut self, message: &str, buttons: &[&str]) -> Option<String>;
    /// Open a new Terminal window running the POSIX `script`.
    fn open_terminal(&mut self, script: &str) -> Result<(), String>;
}

/// The `.app` directory that holds `exe` as `Contents/MacOS/<exe>`.
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let is_app = app
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("app"));
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && is_app)
        .then(|| app.to_path_buf())
}

/// A Finder/LaunchServices start: no arguments (older macOS adds one
/// `-psn_…` process serial number) from inside a `.app` bundle.
pub fn is_app_launch(raw_argv: &[String], exe: &Path) -> bool {
    let bare = match raw_argv {
        [_] => true,
        [_, serial] => serial.starts_with("-psn_"),
        _ => false,
    };
    bare && bundle_of(exe).is_some()
}

/// Run the bundle installer when this process is a double-clicked
/// `Install clud.app`; `None` for every other launch.
pub fn run(args: &Args) -> Option<i32> {
    if !crate::platform::host::MACOS {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    if !is_app_launch(&args.raw_argv, &exe) {
        return None;
    }
    let mut dialogs = crate::platform::app_dialog::NativeDialogs;
    let plan = super::entry::current_install_plan();
    let facts = plan.as_ref().map(Facts::of).map_err(Clone::clone);
    let install = move || match plan {
        Ok(plan) => super::entry::install_consented(plan),
        Err(_) => Outcome::failed(1, "planning_failed"),
    };
    Some(flow(&mut dialogs, facts, &exe, install))
}

/// The dialog sequence; `install` commits the plan `facts` describes.
pub(super) fn flow(
    dialogs: &mut impl Dialogs,
    facts: Result<Facts, String>,
    exe: &Path,
    install: impl FnOnce() -> Outcome,
) -> i32 {
    let facts = match facts {
        Ok(facts) => facts,
        Err(error) => {
            let message = format!("clud cannot prepare the installation:\n\n{error}");
            return rescue(dialogs, exe, &message, 1);
        }
    };
    let proceed = install_label(&facts);
    loop {
        match dialogs
            .choose(&consent_text(&facts), &[NOT_NOW, SHOW_DETAILS, proceed])
            .as_deref()
        {
            Some(label) if label == proceed => break,
            Some(SHOW_DETAILS) => {
                dialogs.choose(&plain_text(&facts.details()), &[BACK]);
            }
            // Not now, or dismissed: nothing was written.
            _ => return 0,
        }
    }
    let outcome = install();
    if outcome.is_complete() {
        let message = format!(
            "clud {} is installed at {}.\n\nOpen a new terminal and type clud.",
            facts.version,
            facts.destination.display()
        );
        if dialogs.choose(&message, &[DONE, OPEN_TERMINAL]).as_deref() == Some(OPEN_TERMINAL) {
            open_or_explain(dialogs, &new_shell_script(), "clud");
        }
        return 0;
    }
    let message = format!(
        "clud {} was not fully installed ({}).\n\nFix in Terminal opens the interactive \
         installer, which shows what is left and can repair it.",
        facts.version,
        outcome.reason().unwrap_or("unknown")
    );
    rescue(dialogs, exe, &message, outcome.exit_code().max(1))
}

/// "Let a human in": offer the interactive installer in Terminal.
fn rescue(dialogs: &mut impl Dialogs, exe: &Path, message: &str, code: i32) -> i32 {
    if dialogs
        .choose(message, &[CLOSE, FIX_IN_TERMINAL])
        .as_deref()
        == Some(FIX_IN_TERMINAL)
    {
        let command = format!("{} --installer", shell_quote(&exe.to_string_lossy()));
        open_or_explain(dialogs, &rescue_script(exe), &command);
    }
    code
}

fn open_or_explain(dialogs: &mut impl Dialogs, script: &str, command: &str) {
    if let Err(error) = dialogs.open_terminal(script) {
        dialogs.choose(
            &format!(
                "Terminal could not be opened ({error}).\n\nRun this in a terminal:\n{command}"
            ),
            &[CLOSE],
        );
    }
}

fn consent_text(facts: &Facts) -> String {
    format!(
        "{}\n\n{}\n\n{}",
        facts.title(),
        plain_text(&facts.summary(&PathStyle::detect())),
        facts.subtitle()
    )
}

/// The proceed button names what the install does to an existing clud.
fn install_label(facts: &Facts) -> &'static str {
    match facts
        .existing_version
        .as_deref()
        .and_then(|existing| compare_versions(existing, &facts.version))
    {
        Some(Ordering::Less) => "Update",
        Some(Ordering::Equal) => "Reinstall",
        _ => "Install",
    }
}

/// POSIX single-quoting: safe for any path, including spaces and quotes.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn rescue_script(exe: &Path) -> String {
    format!(
        "#!/bin/sh\nexec {} --installer\n",
        shell_quote(&exe.to_string_lossy())
    )
}

fn new_shell_script() -> String {
    "#!/bin/sh\nprintf '%s\\n' 'clud is installed. Type clud to start.'\n\
     exec \"${SHELL:-/bin/zsh}\" -l\n"
        .to_owned()
}

#[cfg(test)]
#[path = "app_bundle_tests.rs"]
mod tests;

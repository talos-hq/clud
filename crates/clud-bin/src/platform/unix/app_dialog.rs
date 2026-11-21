//! Native dialogs and the Terminal hand-off for `Install clud.app` (#2863).
//!
//! `osascript`'s `display dialog` runs inside its own process, so it needs no
//! Automation (TCC) permission; the message and button labels travel as
//! `argv`, never spliced into AppleScript source. Terminal is opened through
//! LaunchServices (`open -a Terminal <file>.command`), never driven with
//! `tell application "Terminal"`, which would ask for Automation access.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use crate::backend_bootstrap::run_captured_command;
use crate::self_install::app_bundle::Dialogs;

/// Run `helper flag` to completion; its exit code, or why it did not start.
/// Its output (dialog failures) is relayed to this process's stderr.
pub fn run_helper(helper: &std::path::Path, flag: &str) -> Result<i32, String> {
    let command = vec![helper.to_string_lossy().into_owned(), flag.to_owned()];
    let (code, output) =
        run_captured_command(command).map_err(|error| format!("{}: {error}", helper.display()))?;
    eprint!("{output}");
    Ok(code)
}

/// The production [`Dialogs`]: `osascript` and `open`.
pub struct NativeDialogs;

/// `argv`: title, message, then one to three button labels (the last is
/// the default). Prints the pressed label; empty when dismissed.
const DIALOG_SCRIPT: &[&str] = &[
    "on run argv",
    "set theButtons to items 3 thru -1 of argv",
    // The bundle has no Dock icon (LSUIElement); bring the dialog forward.
    "activate",
    "try",
    "set theReply to display dialog (item 2 of argv) with title (item 1 of argv) \
     buttons theButtons default button (count of theButtons) with icon note",
    "return button returned of theReply",
    "on error number -128",
    "return \"\"",
    "end try",
    "end run",
];

pub fn dialog_command(title: &str, message: &str, buttons: &[&str]) -> Vec<String> {
    let mut command = vec!["osascript".to_owned()];
    for line in DIALOG_SCRIPT {
        command.push("-e".to_owned());
        command.push((*line).to_owned());
    }
    command.push(title.to_owned());
    command.push(message.to_owned());
    command.extend(buttons.iter().map(|label| (*label).to_owned()));
    command
}

impl Dialogs for NativeDialogs {
    fn choose(&mut self, message: &str, buttons: &[&str]) -> Option<String> {
        match run_captured_command(dialog_command("Install clud", message, buttons)) {
            Ok((0, output)) => {
                let label = output.trim();
                (!label.is_empty()).then(|| label.to_owned())
            }
            Ok((code, output)) => {
                eprintln!("clud installer: dialog failed ({code}): {}", output.trim());
                None
            }
            Err(error) => {
                eprintln!("clud installer: dialog failed: {error}");
                None
            }
        }
    }

    fn open_terminal(&mut self, script: &str) -> Result<(), String> {
        // Kept, not deleted on drop: Terminal reads it after `open` returns.
        // Written by this process, so it carries no quarantine attribute.
        let mut file = tempfile::Builder::new()
            .prefix("clud-installer-")
            .suffix(".command")
            .tempfile()
            .map_err(|error| error.to_string())?;
        file.write_all(script.as_bytes())
            .map_err(|error| error.to_string())?;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
        let (_, path) = file.keep().map_err(|error| error.to_string())?;
        let command = vec![
            "open".to_owned(),
            "-a".to_owned(),
            "Terminal".to_owned(),
            path.to_string_lossy().into_owned(),
        ];
        match run_captured_command(command)? {
            (0, _) => Ok(()),
            (code, output) => Err(format!("open exited {code}: {}", output.trim())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_text_travels_as_argv_never_as_applescript_source() {
        let message = "say \"hi\" & do shell script \"x\"";
        let command = dialog_command("Install clud", message, &["Not now", "Install"]);
        assert_eq!(command[0], "osascript");
        let source: Vec<&str> = command
            .iter()
            .skip(1)
            .step_by(2)
            .take(DIALOG_SCRIPT.len())
            .map(String::as_str)
            .collect();
        assert!(source.iter().all(|flag| *flag == "-e"));
        assert_eq!(
            &command[1 + 2 * DIALOG_SCRIPT.len()..],
            ["Install clud", message, "Not now", "Install"]
        );
        assert!(DIALOG_SCRIPT.iter().all(|line| !line.contains("hi")));
    }
}

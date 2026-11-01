//! #2197: external masked-prompt helpers for a Linux desktop where the native
//! egui window cannot load (for example KDE Wayland). Tried in order:
//! `zenity --password`, `kdialog --password`, `ksshaskpass`, then `pinentry`
//! (Assuan `GETPIN`). A helper is only used when it is on PATH and a display
//! exists. The value goes from the helper's stdout straight back to the
//! caller's sink; it is never logged, echoed or placed on a command line.

use std::time::{Duration, Instant};

use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode, StreamKind,
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Helper {
    Zenity,
    Kdialog,
    Ksshaskpass,
    Pinentry,
}

impl Helper {
    const ORDER: [Helper; 4] = [
        Helper::Zenity,
        Helper::Kdialog,
        Helper::Ksshaskpass,
        Helper::Pinentry,
    ];

    fn program(self) -> &'static str {
        match self {
            Helper::Zenity => "zenity",
            Helper::Kdialog => "kdialog",
            Helper::Ksshaskpass => "ksshaskpass",
            Helper::Pinentry => "pinentry",
        }
    }
}

/// Helpers to try, in order, given injected facts (no PATH or env access).
pub fn select_helpers(display: bool, on_path: impl Fn(&str) -> bool) -> Vec<Helper> {
    if !display {
        return Vec::new();
    }
    Helper::ORDER
        .into_iter()
        .filter(|helper| on_path(helper.program()))
        .collect()
}

/// Argument vector after the program name. Prompt text only; never a secret.
fn args(helper: Helper, title: &str, label: &str) -> Vec<String> {
    match helper {
        Helper::Zenity => vec![
            "--password".into(),
            format!("--title={title}"),
            format!("--text={label}"),
        ],
        // `--` ends option parsing so a label starting with `-` stays text.
        Helper::Kdialog => vec![
            "--title".into(),
            title.into(),
            "--password".into(),
            "--".into(),
            label.into(),
        ],
        Helper::Ksshaskpass => vec!["--".into(), label.into()],
        Helper::Pinentry => Vec::new(),
    }
}

fn assuan_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '%' => out.push_str("%25"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            _ => out.push(c),
        }
    }
    out
}

fn pinentry_script(title: &str, label: &str) -> String {
    format!(
        "SETTITLE {}\nSETDESC {}\nSETPROMPT Secret:\nGETPIN\nBYE\n",
        assuan_escape(title),
        assuan_escape(label)
    )
}

/// Decode Assuan `%XX` escapes into raw bytes (UTF-8 is validated by the caller).
fn assuan_unescape(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let decoded = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            if let Some(byte) = decoded {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// What a helper's stdout lines mean once it has exited.
enum Reply {
    Secret(Zeroizing<String>),
    Cancelled,
    /// The helper never answered within the deadline; ends the chain.
    TimedOut,
    Unusable,
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reply::Secret(_) => f.write_str("Secret(<redacted>)"),
            Reply::Cancelled => f.write_str("Cancelled"),
            Reply::TimedOut => f.write_str("TimedOut"),
            Reply::Unusable => f.write_str("Unusable"),
        }
    }
}

impl PartialEq for Reply {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Reply::Secret(a), Reply::Secret(b)) => a.as_bytes() == b.as_bytes(),
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

type Line = Zeroizing<String>;

fn interpret_pinentry(lines: &[Line]) -> Reply {
    let total: usize = lines
        .iter()
        .filter_map(|l| l.strip_prefix("D "))
        .map(str::len)
        .sum();
    let mut data: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::with_capacity(total));
    let mut seen = false;
    for line in lines {
        if let Some(part) = line.strip_prefix("D ") {
            let piece = Zeroizing::new(assuan_unescape(part));
            // Decoded bytes never exceed the escaped length, so `data` never
            // reallocates (which would leave an unwiped copy behind).
            data.extend_from_slice(&piece);
            seen = true;
        }
    }
    if seen {
        return match std::str::from_utf8(&data) {
            Ok(secret) => Reply::Secret(Zeroizing::new(secret.to_owned())),
            Err(_) => Reply::Unusable,
        };
    }
    // 83886179 is gpg-error "Operation cancelled"; 83886278 is
    // GPG_ERR_FULLY_CANCELED.
    if lines
        .iter()
        .any(|l| l.starts_with("ERR 83886179") || l.starts_with("ERR 83886278"))
    {
        Reply::Cancelled
    } else {
        Reply::Unusable
    }
}

/// Toolkit chatter that a working helper prints even on a normal Cancel
/// (for example zenity's `Gtk-WARNING ... mapped without a transient parent`).
const HARMLESS_STDERR: [&str; 8] = [
    "Gtk-WARNING",
    "Gtk-Message",
    "Gdk-",
    "GLib-",
    "qt.qpa.",
    "QStandardPaths",
    "dbind-WARNING",
    "Fontconfig",
];

/// Whether a stderr line is real diagnostics rather than blank or known
/// harmless toolkit noise.
fn is_diagnostic(line: &[u8]) -> bool {
    let text = String::from_utf8_lossy(line);
    !text.trim().is_empty() && !HARMLESS_STDERR.iter().any(|m| text.contains(m))
}

/// `stderr_text` is whether the helper wrote real diagnostics to stderr. A
/// dismissed dialog exits 1 silently (or with only toolkit warnings); exit 1
/// with other diagnostics means it never worked (no display, broken toolkit
/// plugin), so the next helper should be tried.
fn interpret(helper: Helper, exit: i32, lines: &[Line], stderr_text: bool) -> Reply {
    if helper == Helper::Pinentry {
        return interpret_pinentry(lines);
    }
    match (exit, lines.first()) {
        (0, Some(first)) => Reply::Secret(Zeroizing::new(first.to_string())),
        (1, None) if !stderr_text => Reply::Cancelled,
        _ => Reply::Unusable,
    }
}

/// Keep only absolute PATH entries so a relative or empty entry (`.`, `::`)
/// can never resolve a planted helper from the current directory.
fn absolute_path_entries(path_env: &str) -> String {
    path_env
        .split(':')
        .filter(|entry| std::path::Path::new(entry).is_absolute())
        .collect::<Vec<_>>()
        .join(":")
}

fn run_helper(helper: Helper, path: &std::path::Path, title: &str, label: &str) -> Reply {
    let mut argv = vec![path.to_string_lossy().into_owned()];
    argv.extend(args(helper, title, label));
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Pipe,
        stdin_mode: StdinMode::Piped,
        create_process_group: true,
        creationflags: None,
        nice: None,
        address_space_limit_bytes: None,
    });
    if process.start().is_err() {
        return Reply::Unusable;
    }
    if helper == Helper::Pinentry
        && process
            .write_stdin(pinentry_script(title, label).as_bytes())
            .is_err()
    {
        let _ = process.kill();
        return Reply::Unusable;
    }
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut stderr_text = false;
    let mut lines: Vec<Line> = Vec::new();
    let mut closed = [false; 2];
    while !closed.iter().all(|c| *c) && Instant::now() < deadline {
        for (i, stream) in [StreamKind::Stdout, StreamKind::Stderr]
            .into_iter()
            .enumerate()
        {
            if closed[i] {
                continue;
            }
            match process.read_stream(stream, Some(Duration::from_millis(50))) {
                ReadStatus::Line(mut bytes) => {
                    if stream == StreamKind::Stderr {
                        stderr_text |= is_diagnostic(&bytes);
                    } else {
                        let trimmed = bytes.trim_ascii_end();
                        // Owned either way, so the temporary is wiped on drop.
                        let text = Zeroizing::new(String::from_utf8_lossy(trimmed).into_owned());
                        lines.push(text);
                    }
                    bytes.zeroize();
                }
                ReadStatus::Eof => closed[i] = true,
                ReadStatus::Timeout => {}
            }
        }
    }
    // Best effort: drop the crate's retained copies of the secret. The crate
    // does not wipe them (zackees/running-process#1357).
    process.clear_captured_stream(StreamKind::Stdout);
    process.clear_captured_combined();
    if !closed.iter().all(|c| *c) {
        let _ = process.kill();
        return Reply::TimedOut;
    }
    let exit = process
        .wait(Some(deadline.saturating_duration_since(Instant::now())))
        .unwrap_or(-1);
    let _ = process.kill();
    interpret(helper, exit, &lines, stderr_text)
}

/// Try each available helper. `Cancelled` and a timeout end the chain; an unusable helper
/// moves on to the next one.
pub fn prompt(
    title: &str,
    label: &str,
) -> Result<Zeroizing<String>, super::secret_prompt::ExternalFailure> {
    use super::secret_prompt::ExternalFailure;
    let present = |name: &str| {
        std::env::var(name)
            .ok()
            .is_some_and(|value| !value.is_empty())
    };
    let display = present("WAYLAND_DISPLAY") || present("DISPLAY");
    let path_env = absolute_path_entries(&std::env::var("PATH").unwrap_or_default());
    let found = |name: &str| crate::shim_resolve::which(name, &path_env);
    let helpers = select_helpers(display, |name| found(name).is_some());
    if !helpers.is_empty() {
        eprintln!("clud: waiting for a desktop prompt to collect the secret...");
    }
    for helper in helpers {
        let Some(path) = found(helper.program()) else {
            continue;
        };
        match run_helper(helper, &path, title, label) {
            Reply::Secret(value) => return Ok(value),
            Reply::Cancelled | Reply::TimedOut => return Err(ExternalFailure::Cancelled),
            Reply::Unusable => {}
        }
    }
    Err(ExternalFailure::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(text: &str) -> Line {
        Zeroizing::new(text.to_string())
    }

    #[test]
    fn selection_follows_fixed_order_and_requires_path_and_display() {
        let all = |_: &str| true;
        assert_eq!(select_helpers(true, all), Helper::ORDER.to_vec());
        assert_eq!(
            select_helpers(true, |n| n == "kdialog" || n == "pinentry"),
            vec![Helper::Kdialog, Helper::Pinentry]
        );
        assert!(select_helpers(false, all).is_empty());
        assert!(select_helpers(true, |_| false).is_empty());
    }

    #[test]
    fn prompt_text_is_never_a_secret_and_goes_to_the_right_argument() {
        assert_eq!(
            args(Helper::Ksshaskpass, "t", "Token for host"),
            vec!["--", "Token for host"]
        );
        assert!(args(Helper::Zenity, "t", "l").contains(&l("--password")));
        assert!(args(Helper::Pinentry, "t", "l").is_empty());
    }

    #[test]
    fn pinentry_script_escapes_and_replies_unescape() {
        let script = pinentry_script("a%b", "x\ny");
        assert!(script.contains("SETTITLE a%25b\n"));
        assert!(script.contains("SETDESC x%0Ay\n"));
        let lines = vec![l("D p%25w%0A"), l("OK")];
        assert_eq!(
            interpret(Helper::Pinentry, 0, &lines, false),
            Reply::Secret(l("p%w\n"))
        );
        let cancel = vec![l("ERR 83886179 Operation cancelled")];
        assert_eq!(
            interpret(Helper::Pinentry, 0, &cancel, false),
            Reply::Cancelled
        );
        assert_eq!(interpret(Helper::Pinentry, 0, &[], false), Reply::Unusable);
    }

    #[test]
    fn simple_helper_exit_codes_map_to_outcomes() {
        let secret = vec![l("hunter2")];
        assert_eq!(
            interpret(Helper::Zenity, 0, &secret, false),
            Reply::Secret(l("hunter2"))
        );
        assert_eq!(interpret(Helper::Zenity, 1, &[], false), Reply::Cancelled);
        assert_eq!(interpret(Helper::Kdialog, 127, &[], false), Reply::Unusable);
        assert_eq!(
            interpret(Helper::Ksshaskpass, 0, &[], false),
            Reply::Unusable
        );
    }

    #[test]
    fn exit_one_with_stderr_is_unusable_not_cancel() {
        assert_eq!(interpret(Helper::Zenity, 1, &[], true), Reply::Unusable);
        assert_eq!(interpret(Helper::Kdialog, 1, &[], true), Reply::Unusable);
    }

    #[test]
    fn real_gtk_warning_on_cancel_is_still_a_cancel() {
        let gtk = b"(zenity:4242): Gtk-WARNING **: 12:00:00.000: GtkDialog mapped without a transient parent. This is discouraged.";
        assert!(!is_diagnostic(gtk));
        assert!(!is_diagnostic(b"   "));
        assert!(!is_diagnostic(
            b"qt.qpa.plugin: Could not find the Qt platform plugin"
        ));
        assert!(is_diagnostic(b"cannot open display:"));
        assert_eq!(interpret(Helper::Zenity, 1, &[], false), Reply::Cancelled);
    }

    #[test]
    fn fully_canceled_and_debug_redaction() {
        let cancel = vec![l("ERR 83886278 Operation fully canceled")];
        assert_eq!(
            interpret(Helper::Pinentry, 0, &cancel, false),
            Reply::Cancelled
        );
        let shown = format!("{:?}", Reply::Secret(l("hunter2")));
        assert!(!shown.contains("hunter2"));
    }

    #[test]
    fn relative_and_empty_path_entries_are_dropped() {
        assert_eq!(
            absolute_path_entries(".:/usr/bin::bin:/opt/x:"),
            "/usr/bin:/opt/x"
        );
        assert_eq!(absolute_path_entries(":"), "");
    }

    #[test]
    fn pinentry_utf8_and_multi_line_data_round_trip() {
        // "pässwörd" split over two D lines, UTF-8 bytes percent-escaped.
        let lines = vec![l("D p%C3%A4ssw"), l("D %C3%B6rd"), l("OK")];
        assert_eq!(
            interpret(Helper::Pinentry, 0, &lines, false),
            Reply::Secret(l("pässwörd"))
        );
        let bad = vec![l("D %FF")];
        assert_eq!(interpret(Helper::Pinentry, 0, &bad, false), Reply::Unusable);
    }
}

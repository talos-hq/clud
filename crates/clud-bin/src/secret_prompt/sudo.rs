//! `clud sudo -- <command>`: the one way an agent obtains a sudo password.
//!
//! An agent's shell tool runs each call in a new session with no controlling
//! terminal, so sudo keys any ticket on the *parent process* (sudoers
//! `timestamp_type=tty` falls back to `ppid` without a tty). A ticket made in
//! one tool call is therefore never visible to the next one, and a ticket made
//! by a helper process is never visible to its caller. This process is the
//! parent of every sudo it runs, which is what makes one prompt cover the
//! command. See `docs/architecture/secret-catalog.md#sudo`.
//!
//! The password goes from lokr's masked prompt to sudo's stdin pipe only. It
//! is never in argv, the environment, or relayed output. `-A`/`SUDO_ASKPASS`
//! is not used because sudo-rs rejects `-A`; `-S` works on classic sudo,
//! sudo-rs and macOS sudo alike.

use std::io::Write;
use std::time::{Duration, Instant};

use clap::Parser;
use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode, StreamKind,
};
use zeroize::{Zeroize, Zeroizing};

use super::{Failure, Options, Secret};

/// Prompts per invocation, matching sudo's default `passwd_tries`.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
const SUDO: &str = "sudo";
/// How long sudo may take to answer (PAM delays, lecture); user think time
/// at the prompt is not counted.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(120);
/// sudo prints this (`-S -p`) each time it reads a password, on every flavour.
const PASSWORD_REQUEST: &str = "clud-sudo-password-request";

/// Run one command as root, acquiring the sudo password through lokr's
/// masked prompt only when sudo needs one.
#[derive(Debug, Clone, Parser)]
pub struct SudoOptions {
    /// Why root is needed; shown in the masked prompt.
    #[arg(long, visible_alias = "message", value_name = "TEXT")]
    pub reason: Option<String>,
    /// Print the prompt text and the sudo invocations without prompting or running.
    #[arg(long)]
    pub dry_run: bool,
    /// The command, optionally preceded by sudo options such as `-u USER`.
    #[arg(
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    pub command: Vec<String>,
}

/// Long sudo options that take a value as the next word.
const VALUE_OPTIONS: &[&str] = &[
    "--user",
    "--group",
    "--host",
    "--close-from",
    "--chroot",
    "--directory",
    "--command-timeout",
    "--role",
    "--type",
    "--other-user",
];

/// Short options whose value may follow in the same word (`-upostgres`).
const VALUE_SHORT: &[char] = &['u', 'g', 'h', 'C', 'T', 'R', 'D', 'U', 'r', 't'];

/// Options clud owns (`-S -p '' -v`, `-n`, `-k`) or that never run a command.
const RESERVED_SHORT: &[char] = &['A', 'S', 'p', 'k', 'K', 'v', 'l', 'e', 'V'];
const RESERVED_LONG: &[&str] = &[
    "--askpass",
    "--stdin",
    "--prompt",
    "--reset-timestamp",
    "--remove-timestamp",
    "--validate",
    "--list",
    "--edit",
    "--version",
];

/// Reject options clud sets itself, and an empty command.
pub(crate) fn check_command(argv: &[String]) -> Result<(), String> {
    let mut index = 0;
    while let Some(word) = argv.get(index) {
        if word == "--" {
            index += 1;
            break;
        }
        if !word.starts_with('-') || word == "-" {
            break;
        }
        let name = word.split('=').next().unwrap_or(word);
        let reserved = if name.starts_with("--") {
            RESERVED_LONG.contains(&name)
        } else {
            // `-ukevin`: the rest of a cluster after a value option is its value.
            word.chars()
                .skip(1)
                .take_while(|flag| !VALUE_SHORT.contains(flag))
                .any(|flag| RESERVED_SHORT.contains(&flag))
        };
        if reserved {
            return Err(format!(
                "`{word}` is managed by clud sudo (it supplies the password through `-S` and never prompts on a terminal); remove it"
            ));
        }
        let shell = matches!(name, "-i" | "-s" | "--login" | "--shell");
        let takes_next = if name.starts_with("--") {
            VALUE_OPTIONS.contains(&name) && !word.contains('=')
        } else {
            word.chars()
                .skip(1)
                .position(|flag| VALUE_SHORT.contains(&flag))
                .is_some_and(|at| at + 2 == word.chars().count())
        };
        index += if takes_next { 2 } else { 1 };
        if shell && argv.get(index).is_none() {
            return Err("an interactive root shell cannot run under an agent; name the command to run".into());
        }
    }
    if argv.get(index).is_none() {
        return Err("missing command: use `clud sudo -- <command> [args...]`".into());
    }
    Ok(())
}

fn user_name() -> String {
    ["USER", "LOGNAME"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "this user".into())
}

fn host_name() -> String {
    sysinfo::System::host_name().unwrap_or_else(|| "this computer".into())
}

/// The masked prompt for attempt `attempt` (1-based).
pub(crate) fn prompt_options(options: &SudoOptions, attempt: u32) -> Options {
    let title = format!("sudo password for {} on {}", user_name(), host_name());
    let purpose = options.reason.as_deref().map_or_else(
        || format!("Run as root: {}", shell_words::join(&options.command)),
        |reason| reason.trim().to_owned(),
    );
    let retry = if attempt > 1 {
        format!("sudo rejected the previous password (attempt {attempt} of {MAX_ATTEMPTS}). ")
    } else {
        String::new()
    };
    Options {
        internal_vault_stdin: false,
        title,
        label: format!(
            "{retry}{purpose}. This is your account password for sudo; clud passes it only to sudo's stdin for this one command."
        ),
        hint: None,
        dry_run: false,
        multiline: false,
        pattern: None,
        to_file: None,
        to_keyring: Vec::new(),
        vault: None,
        to_command: Vec::new(),
    }
}

enum Probe {
    Ready,
    NeedsPassword,
    Denied(String),
}

/// sudo's own refusal (not a password problem): no prompt can fix it.
pub(crate) fn is_denial(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "may not run sudo",
        "not in the sudoers",
        "i'm sorry",
        "is not allowed to",
        "not allowed to execute",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// A sudo whose stdout and stderr are captured together, never relayed.
fn captured_sudo(args: &[&str]) -> NativeProcess {
    let mut argv = vec![SUDO.to_string()];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    // English diagnostics so a refusal can be recognized; only these probes.
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| !matches!(key.as_str(), "LC_ALL" | "LC_MESSAGES" | "LANG" | "LANGUAGE"))
        .collect();
    env.push(("LC_ALL".into(), "C".into()));
    NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: Some(env),
        capture: true,
        stderr_mode: StderrMode::Stdout,
        stdin_mode: StdinMode::Piped,
        create_process_group: false,
        creationflags: None,
        nice: None,
        address_space_limit_bytes: None,
    })
}

/// Run sudo with captured, discarded output and empty stdin.
fn quiet_sudo(args: &[&str]) -> Result<(i32, String), Failure> {
    let process = captured_sudo(args);
    process.start().map_err(|_| Failure::Unavailable)?;
    process.write_stdin(b"").map_err(|_| Failure::Sink)?;
    let deadline = Instant::now() + VALIDATE_TIMEOUT;
    let mut text = String::new();
    loop {
        match process.read_stream(
            StreamKind::Stdout,
            Some(deadline.saturating_duration_since(Instant::now())),
        ) {
            ReadStatus::Line(mut line) => {
                if text.len() < 4096 {
                    text.push_str(&String::from_utf8_lossy(&line));
                    text.push('\n');
                }
                line.zeroize();
            }
            ReadStatus::Eof => break,
            ReadStatus::Timeout => {
                let _ = process.kill();
                return Err(Failure::Sink);
            }
        }
    }
    let code = process
        .wait(Some(deadline.saturating_duration_since(Instant::now())))
        .map_err(|_| Failure::Sink)?;
    Ok((code, text))
}

fn probe() -> Result<Probe, Failure> {
    let (code, text) = quiet_sudo(&["-n", "-v"])?;
    Ok(if code == 0 {
        Probe::Ready
    } else if is_denial(&text) {
        Probe::Denied(text.trim().to_owned())
    } else {
        Probe::NeedsPassword
    })
}

fn password_line(secret: &Secret) -> Zeroizing<Vec<u8>> {
    let mut line = Zeroizing::new(Vec::with_capacity(secret.len() + 1));
    line.extend_from_slice(secret.as_bytes());
    line.push(b'\n');
    line
}

/// The outcome of one `sudo -S -v` session.
enum Auth {
    /// sudo accepted; the password, if sudo asked for one.
    Granted(Option<Secret>),
    /// sudo rejected every password the user gave.
    Rejected(u32),
}

/// One `sudo -S -p <marker> -v` session. sudo prints the marker each time it
/// reads a password, and each request opens one masked prompt. Keeping stdin
/// open until sudo exits means a wrong password costs exactly one PAM failure:
/// closing it after one line makes sudo read an empty password too, which
/// `pam_faillock` (Arch's default) counts against the account.
fn authenticate(options: &SudoOptions) -> Result<Auth, Failure> {
    let marker = format!("{PASSWORD_REQUEST}\n");
    let process = captured_sudo(&["-S", "-p", &marker, "-v"]);
    process.start().map_err(|_| Failure::Unavailable)?;
    let mut attempts = 0;
    let mut last: Option<Secret> = None;
    loop {
        match process.read_stream(StreamKind::Stdout, Some(VALIDATE_TIMEOUT)) {
            ReadStatus::Line(mut line) => {
                let asks = String::from_utf8_lossy(&line).contains(PASSWORD_REQUEST);
                line.zeroize();
                if !asks {
                    continue;
                }
                if attempts == MAX_ATTEMPTS {
                    let _ = process.kill();
                    return Ok(Auth::Rejected(attempts));
                }
                attempts += 1;
                let secret = super::acquire_with(&prompt_options(options, attempts), super::acquire)
                    .inspect_err(|_| {
                        let _ = process.kill();
                    })?;
                if process.write_stdin_streaming(&password_line(&secret)).is_err() {
                    let _ = process.kill();
                    return Err(Failure::Sink);
                }
                last = Some(secret);
            }
            ReadStatus::Eof => break,
            ReadStatus::Timeout => {
                let _ = process.kill();
                return Err(Failure::Sink);
            }
        }
    }
    let code = process.wait(Some(VALIDATE_TIMEOUT)).map_err(|_| Failure::Sink)?;
    Ok(if code == 0 {
        Auth::Granted(last)
    } else {
        Auth::Rejected(attempts)
    })
}

/// Replace any occurrence of the password in relayed output.
pub(crate) fn redact(line: &[u8], secret: &[u8]) -> Vec<u8> {
    if secret.is_empty() || line.len() < secret.len() {
        return line.to_vec();
    }
    let mut out = Vec::with_capacity(line.len());
    let mut index = 0;
    while index < line.len() {
        if line[index..].starts_with(secret) {
            out.extend_from_slice(b"[redacted]");
            index += secret.len();
        } else {
            out.push(line[index]);
            index += 1;
        }
    }
    out
}

/// Run the privileged command, relaying its output line by line.
fn run_privileged(command: &[String], secret: Option<&Secret>) -> i32 {
    let mut argv = vec![SUDO.to_string()];
    match secret {
        Some(_) => argv.extend(["-S".into(), "-p".into(), String::new()]),
        None => argv.push("-n".into()),
    }
    argv.extend(command.iter().cloned());
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Pipe,
        stdin_mode: if secret.is_some() {
            StdinMode::Piped
        } else {
            StdinMode::Inherit
        },
        create_process_group: false,
        creationflags: None,
        nice: None,
        address_space_limit_bytes: None,
    });
    if process.start().is_err() {
        eprintln!("clud sudo: could not start sudo");
        return 1;
    }
    if let Some(secret) = secret {
        // sudo reads one line; the command then sees end of input.
        if process.write_stdin(&password_line(secret)).is_err() {
            let _ = process.kill();
            eprintln!("clud sudo: could not hand the password to sudo");
            return 1;
        }
    }
    let empty = Zeroizing::new(String::new());
    let secret_bytes = secret.unwrap_or(&empty).as_bytes();
    loop {
        match process.read_combined(None) {
            ReadStatus::Line(mut event) => {
                let line = redact(&event.line, secret_bytes);
                event.line.zeroize();
                let _ = match event.stream {
                    StreamKind::Stdout => write_line(&mut std::io::stdout().lock(), &line),
                    StreamKind::Stderr => write_line(&mut std::io::stderr().lock(), &line),
                };
            }
            ReadStatus::Eof => break,
            ReadStatus::Timeout => {}
        }
    }
    process.wait(None).unwrap_or(1)
}

fn write_line(writer: &mut dyn Write, line: &[u8]) -> std::io::Result<()> {
    writer.write_all(line)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn dry_run(options: &SudoOptions) -> i32 {
    let prompt = prompt_options(options, 1);
    let command = shell_words::join(&options.command);
    println!(
        "{}\n{}\nPrompt engine: {}\nIf sudo needs no password: sudo -n {command}\nOtherwise: sudo -S -v, one masked prompt per password sudo asks for (at most 3), then sudo -n {command} (or sudo -S -p '' {command} when sudo keeps no ticket)",
        prompt.title,
        prompt.label,
        crate::managed_lokr::preview()
    );
    0
}

fn report_failure(error: Failure) -> i32 {
    match error {
        Failure::Cancelled => {
            eprintln!("clud sudo: cancelled; nothing was run.");
            3
        }
        Failure::Unavailable => {
            eprintln!("clud sudo: no masked prompt is available (lokr needs a desktop session or a terminal). Do not ask for the password in chat; report that sudo could not be authorized here.");
            1
        }
        _ => {
            eprintln!("clud sudo: could not acquire the password; nothing was run.");
            1
        }
    }
}

/// `clud sudo [--reason TEXT] -- <command>`.
pub fn run(options: &SudoOptions) -> i32 {
    if crate::platform::host::WINDOWS {
        eprintln!("clud sudo: sudo is a Unix tool; on Windows run the command from an elevated terminal.");
        return 2;
    }
    if let Err(message) = check_command(&options.command) {
        eprintln!("clud sudo: {message}");
        return 2;
    }
    if options.dry_run {
        return dry_run(options);
    }
    match probe() {
        Ok(Probe::Ready) => return finish(run_privileged(&options.command, None)),
        Ok(Probe::Denied(text)) => {
            eprintln!("{text}\nclud sudo: sudo refuses this account; a password cannot help, so no prompt was opened.");
            return 1;
        }
        Ok(Probe::NeedsPassword) => {}
        Err(_) => {
            eprintln!("clud sudo: sudo is not available on this system");
            return 1;
        }
    }
    match authenticate(options) {
        Ok(Auth::Granted(secret)) => finish(run_validated(&options.command, secret.as_ref())),
        Ok(Auth::Rejected(attempts)) => {
            eprintln!("clud sudo: sudo rejected the password {attempts} times; nothing was run.");
            finish(1)
        }
        Err(error) => report_failure(error),
    }
}

/// Prefer the ticket this process now holds; fall back to `-S` when sudo
/// keeps none (`timestamp_timeout=0`).
fn run_validated(command: &[String], secret: Option<&Secret>) -> i32 {
    match (quiet_sudo(&["-n", "-v"]), secret) {
        (Ok((0, _)), _) | (_, None) => run_privileged(command, None),
        (_, Some(secret)) => run_privileged(command, Some(secret)),
    }
}

/// Drop the ticket this process created; it is useless to anyone else.
fn finish(code: i32) -> i32 {
    let _ = quiet_sudo(&["-k"]);
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn command_check_accepts_plain_commands_and_target_options() {
        for argv in [
            &["apt-get", "install", "-y", "foo"][..],
            &["-u", "postgres", "psql", "-c", "select 1"],
            &["--user=postgres", "psql"],
            &["-E", "-H", "make", "install"],
            &["-Eu", "postgres", "psql"],
            &["-ukevin", "id"],
            &["--", "id", "-u"],
            &["-i", "systemctl", "restart", "nginx"],
        ] {
            assert!(check_command(&words(argv)).is_ok(), "{argv:?}");
        }
    }

    #[test]
    fn command_check_refuses_clud_owned_options_and_empty_commands() {
        for argv in [
            &[][..],
            &["-u", "postgres"],
            &["-A", "id"],
            &["-S", "id"],
            &["-nS", "id"],
            &["-p", "x", "id"],
            &["-k", "id"],
            &["-v"],
            &["-l"],
            &["--askpass", "id"],
            &["--stdin", "id"],
            &["--prompt=x", "id"],
            &["-i"],
            &["-s"],
        ] {
            assert!(check_command(&words(argv)).is_err(), "{argv:?}");
        }
    }

    #[test]
    fn prompt_names_account_host_purpose_and_retry() {
        let options = SudoOptions::try_parse_from(["sudo", "--", "apt-get", "install", "-y", "foo"])
            .unwrap();
        let first = prompt_options(&options, 1);
        assert!(first.title.starts_with("sudo password for "));
        assert!(first.label.starts_with("Run as root: apt-get install -y foo."));
        assert!(super::super::parse_context(&first.title).is_ok());
        let retry = prompt_options(&options, 2);
        assert!(retry.label.starts_with("sudo rejected the previous password (attempt 2 of 3)."));
        let reasoned = SudoOptions::try_parse_from([
            "sudo",
            "--reason",
            "Install the foo package",
            "--",
            "apt-get",
            "install",
        ])
        .unwrap();
        assert!(prompt_options(&reasoned, 1)
            .label
            .starts_with("Install the foo package."));
    }

    #[test]
    fn denial_text_is_recognized_on_both_sudo_flavours() {
        assert!(is_denial("Sorry, user bob may not run sudo on host."));
        assert!(is_denial("bob is not in the sudoers file."));
        assert!(is_denial("sudo-rs: I'm sorry bob. I'm afraid I can't do that"));
        assert!(!is_denial("sudo: a password is required"));
        assert!(!is_denial("sudo-rs: interactive authentication is required"));
    }

    #[test]
    fn relayed_output_never_contains_the_password() {
        assert_eq!(
            redact(b"echo pw-123 and pw-123", b"pw-123"),
            b"echo [redacted] and [redacted]".to_vec()
        );
        assert_eq!(redact(b"plain", b"pw-123"), b"plain".to_vec());
        assert_eq!(redact(b"plain", b""), b"plain".to_vec());
    }

    #[test]
    fn password_reaches_sudo_as_one_stdin_line() {
        let line = password_line(&Zeroizing::new("synthetic".into()));
        assert_eq!(&line[..], b"synthetic\n");
    }
}

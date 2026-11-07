//! #1708: acquire credentials in a masked window and deliver directly to a sink.
//! Secret bytes have no Debug, serialization, command-line or status surface.

use std::path::PathBuf;

use clap::{ArgGroup, Parser, ValueEnum};
use zeroize::Zeroizing;

#[path = "secret_prompt/gui.rs"]
mod gui;
#[path = "secret_prompt/sink.rs"]
mod sink;

/// Public options shared by `clud secret` and its multicall alias.
#[derive(Debug, Clone, Parser)]
#[command(group(ArgGroup::new("sink").required(true).args(["to_file", "vault", "to_command"])))]
pub struct Options {
    /// Window title identifying the credential request.
    #[arg(long, default_value = "clud credential request")]
    pub title: String,
    /// Describe the credential and host to the user.
    #[arg(long, default_value = "Enter the requested credential")]
    pub label: String,
    /// Accept a masked multiline value, such as a private key.
    #[arg(long)]
    pub multiline: bool,
    /// Require the entire value to match this regex; retry up to three times.
    #[arg(long, value_name = "REGEX", value_parser = parse_pattern)]
    pub pattern: Option<regex::Regex>,
    /// Write to an owner-only file (0600 on Unix, protected DACL on Windows).
    #[arg(long, value_name = "PATH")]
    pub to_file: Option<PathBuf>,
    /// Store a provider credential in clud's native vault.
    #[arg(long, value_enum)]
    pub vault: Option<Vault>,
    /// Deliver the credential to this command's stdin; suppress its output.
    #[arg(long, num_args = 1.., allow_hyphen_values = true, value_name = "COMMAND")]
    pub to_command: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Vault {
    Deepseek,
    Kimi,
    Openrouter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Failure {
    Cancelled,
    Unavailable,
    Invalid,
    Sink,
    Pattern,
}

type Secret = Zeroizing<String>;

fn parse_pattern(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::Regex::new(pattern)?;
    regex::Regex::new(&format!("\\A(?:{pattern})\\z")).or_else(|_| {
        // A trailing free-spacing comment can consume the closing group.
        regex::Regex::new(&format!("\\A(?:{pattern}\n)\\z"))
    })
}

/// Self-contained dispatch: no harness, daemon, transcript or model call.
pub fn run(options: &Options) -> i32 {
    let result = acquire_with(options, acquire).and_then(|secret| sink::deliver(options, &secret));
    report(result)
}

fn report(result: Result<(), Failure>) -> i32 {
    match result {
        Ok(()) => {
            println!("stored");
            0
        }
        Err(Failure::Cancelled) => {
            println!("cancelled");
            3
        }
        Err(error) => {
            println!("failed");
            eprintln!(
                "{}",
                match error {
                    Failure::Unavailable =>
                        "No graphical secret prompt is available; use a desktop with its native graphics libraries installed.",
                    Failure::Pattern => "The credential does not match the expected format after three attempts.",
                    Failure::Invalid =>
                        "The credential is empty, too large or invalid for this input mode.",
                    _ => "Could not deliver the credential to the requested sink.",
                }
            );
            1
        }
    }
}

fn acquire_with(
    options: &Options,
    mut prompt: impl FnMut(&Options) -> Result<Secret, Failure>,
) -> Result<Secret, Failure> {
    let mut retry = options.clone();
    for _ in 0..3 {
        let secret = validate_secret(prompt(&retry)?, options.multiline);
        let Some(pattern) = &options.pattern else {
            return secret;
        };
        if let Ok(secret) = secret {
            if pattern.is_match(&secret) {
                return Ok(secret);
            }
        }
        retry.label = format!(
            "Value does not match the expected format.\n{}",
            options.label
        );
    }
    Err(Failure::Pattern)
}

fn acquire(options: &Options) -> Result<Secret, Failure> {
    if test_mode() {
        if std::env::var_os("CLUD_TEST_SECRET_PROMPT_CANCEL").is_some() {
            return Err(Failure::Cancelled);
        }
        let fixture = std::env::var_os("CLUD_TEST_SECRET_PROMPT_FILE")
            .filter(|_| std::env::var_os("CLUD_TEST_SECRET_PROMPT_GUI").is_none());
        if let Some(path) = fixture {
            let text = std::fs::read_to_string(path).map_err(|_| Failure::Unavailable)?;
            return Ok(Zeroizing::new(text));
        }
    }
    gui::prompt(options)
}

fn validate_secret(secret: Secret, multiline: bool) -> Result<Secret, Failure> {
    if secret.is_empty() || secret.len() > 1024 * 1024 || secret.contains('\0') {
        return Err(Failure::Invalid);
    }
    if !multiline && secret.contains(['\r', '\n']) {
        return Err(Failure::Invalid);
    }
    Ok(secret)
}

fn test_mode() -> bool {
    cfg!(debug_assertions)
        && std::env::var_os("CLUD_INTEGRATION_TESTS").is_some_and(|value| value == "1")
}

/// The multicall prompt has exactly the same validated sink options.
pub fn run_alias(argv: &[std::ffi::OsString]) -> i32 {
    match Options::try_parse_from(argv) {
        Ok(options) => run(&options),
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            code
        }
    }
}

/// Askpass is a separate, caller-checked protocol, never the agent-facing CLI.
pub fn run_askpass(argv: &[std::ffi::OsString]) -> i32 {
    if !sink::askpass_caller_allowed() {
        eprintln!("failed: askpass must be invoked by git, ssh or sudo");
        return 2;
    }
    let options = Options {
        title: "clud authentication".to_string(),
        label: argv.get(1).map_or_else(
            || "Enter the requested credential".to_string(),
            |value| value.to_string_lossy().into_owned(),
        ),
        multiline: false,
        pattern: None,
        to_file: None,
        vault: None,
        to_command: Vec::new(),
    };
    match acquire_with(&options, acquire) {
        Ok(secret) => {
            use std::io::Write;
            // Only the authenticated calling tool receives this protocol output.
            let mut stdout = std::io::stdout().lock();
            if stdout
                .write_all(secret.as_bytes())
                .and_then(|()| stdout.write_all(b"\n"))
                .is_ok()
            {
                0
            } else {
                1
            }
        }
        Err(error) => {
            eprintln!(
                "{}",
                if error == Failure::Cancelled {
                    "cancelled"
                } else {
                    "failed"
                }
            );
            if error == Failure::Cancelled {
                3
            } else {
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_pattern_rejects_before_storing() {
        let options =
            Options::try_parse_from(["secret", "--pattern", "[0-9]+", "--to-file", "unused"])
                .unwrap();
        let mut stored = false;
        let result = acquire_with(&options, |_| {
            Ok(Zeroizing::new("synthetic-mismatch".into()))
        })
        .map(|_| stored = true);
        assert!(!stored, "mismatching value reached the sink");
        assert!(result.is_err());
    }

    #[test]
    fn secret_pattern_retries_then_delivers_once() {
        let options = Options::try_parse_from([
            "secret",
            "--label",
            "Token for test host",
            "--pattern",
            "[0-9]+",
            "--to-file",
            "unused",
        ])
        .unwrap();
        let mut attempts = 0;
        let mut stored = 0;
        let result = acquire_with(&options, |prompt| {
            attempts += 1;
            assert!(prompt.label.contains(&options.label));
            if attempts > 1 {
                assert!(prompt
                    .label
                    .starts_with("Value does not match the expected format.\n"));
            }
            Ok(Zeroizing::new(
                if attempts < 3 {
                    "synthetic-mismatch"
                } else {
                    "123"
                }
                .into(),
            ))
        })
        .map(|_| stored += 1);
        assert!(result.is_ok());
        assert_eq!(attempts, 3);
        assert_eq!(stored, 1);
    }

    #[test]
    fn secret_pattern_exhaustion_and_cancellation_never_deliver() {
        let options =
            Options::try_parse_from(["secret", "--pattern", "[0-9]+", "--to-file", "unused"])
                .unwrap();
        let mut attempts = 0;
        let result = acquire_with(&options, |_| {
            attempts += 1;
            Ok(Zeroizing::new("synthetic-mismatch".into()))
        });
        assert!(matches!(result, Err(Failure::Pattern)));
        assert_eq!(attempts, 3);
        attempts = 0;
        let result = acquire_with(&options, |_| {
            attempts += 1;
            Err(Failure::Cancelled)
        });
        assert!(matches!(result, Err(Failure::Cancelled)));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn secret_pattern_is_full_value_and_invalid_regex_is_typed() {
        let pattern = parse_pattern("[0-9]+|abc").unwrap();
        assert!(!pattern.is_match("x123"));
        assert!(!pattern.is_match("123x"));
        assert!(!pattern.is_match("123\n"));
        assert!(pattern.is_match("abc"));
        let commented = parse_pattern("(?x)[0-9]+ # digits").unwrap();
        assert!(commented.is_match("123"));
        assert!(!commented.is_match("123x"));
        assert!(parse_pattern("(?s).*")
            .unwrap()
            .is_match("synthetic\nfixture\n"));
        let error = Options::try_parse_from(["secret", "--pattern", "[", "--to-file", "unused"])
            .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn secret_pattern_absent_preserves_single_attempt_behavior() {
        let options = Options::try_parse_from(["secret", "--to-file", "unused"]).unwrap();
        let mut attempts = 0;
        let result = acquire_with(&options, |_| {
            attempts += 1;
            Ok(Zeroizing::new("synthetic-mismatch".into()))
        });
        assert!(result.is_ok());
        assert_eq!(attempts, 1);
    }

    #[test]
    fn secret_validation_preserves_multiline_and_refuses_invalid_values() {
        let text = "private\nkey\n";
        assert_eq!(
            &*validate_secret(Zeroizing::new(text.into()), true).unwrap(),
            text
        );
        assert!(validate_secret(Zeroizing::new(text.into()), false).is_err());
        assert!(validate_secret(Zeroizing::new(String::new()), true).is_err());
        assert!(validate_secret(Zeroizing::new("a\0b".into()), true).is_err());
    }

    #[test]
    fn a_sink_is_required_and_mutually_exclusive() {
        assert!(Options::try_parse_from(["secret"]).is_err());
        assert!(Options::try_parse_from(["secret", "--to-file", "x", "--vault", "kimi"]).is_err());
        let parsed = Options::try_parse_from([
            "secret",
            "--to-command",
            "gh",
            "auth",
            "login",
            "--with-token",
        ])
        .unwrap();
        assert_eq!(parsed.to_command, ["gh", "auth", "login", "--with-token"]);
    }
}

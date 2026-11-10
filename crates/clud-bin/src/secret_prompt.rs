//! #1708: acquire credentials in a masked window and deliver directly to a sink.
//! Secret bytes have no Debug, serialization, command-line or status surface.

use std::path::PathBuf;

use clap::{ArgGroup, Parser, ValueEnum};
use zeroize::Zeroizing;

#[path = "secret_prompt/gui.rs"]
mod gui;
#[path = "secret_prompt/lokr.rs"]
mod lokr;
#[path = "secret_prompt/sink.rs"]
mod sink;

/// Public options shared by `clud secret` and its multicall alias.
#[derive(Debug, Clone, Parser)]
#[command(group(ArgGroup::new("sink").required(true).args(["to_file", "vault", "to_command", "to_keyring"])))]
pub struct Options {
    /// Internal stdin-only vault sink used by lokr.
    #[arg(long, hide = true, requires = "vault", conflicts_with = "dry_run")]
    internal_vault_stdin: bool,
    /// Name the specific credential and service/host (for example GitHub OAuth Client ID for example.com).
    #[arg(long, value_parser = parse_context)]
    pub title: String,
    /// Explain why it is needed and where to find it.
    #[arg(long, visible_alias = "message", value_parser = parse_context)]
    pub label: String,
    /// Expected format, displayed alongside the request.
    #[arg(long)]
    pub hint: Option<String>,
    /// Preview the request text and destination without acquiring or storing a secret.
    #[arg(long)]
    pub dry_run: bool,
    /// Accept a masked multiline value, such as a private key.
    #[arg(long)]
    pub multiline: bool,
    /// Require the entire value to match this regex; lokr limits submissions to three.
    #[arg(long, value_name = "REGEX", value_parser = parse_pattern)]
    pub pattern: Option<regex::Regex>,
    /// Write to an owner-only file (0600 on Unix, protected DACL on Windows).
    #[arg(long, value_name = "PATH")]
    pub to_file: Option<PathBuf>,
    /// Store in lokr’s native keyring under SERVICE and USER.
    #[arg(long, num_args = 2, value_names = ["SERVICE", "USER"])]
    pub to_keyring: Vec<String>,
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

const LOGIN_WARNING: &str = "This is NOT your computer or login password.";

fn parse_context(value: &str) -> Result<String, String> {
    let words: Vec<String> = value
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    let generic = [
        "please",
        "enter",
        "provide",
        "type",
        "paste",
        "your",
        "the",
        "a",
        "an",
        "requested",
        "request",
        "clud",
        "password",
        "passwd",
        "secret",
        "credential",
        "credentials",
        "token",
        "api",
        "key",
        "authentication",
        "auth",
        "input",
        "required",
        "here",
        "for",
        "system",
        "computer",
        "login",
        "my",
        "now",
        "value",
        "needed",
        "to",
        "continue",
        "below",
        "prompt",
        "supply",
        "authenticate",
        "access",
        "this",
        "operation",
        "it",
        "is",
        "account",
    ];
    if words.is_empty() || words.iter().all(|word| generic.contains(&word.as_str())) {
        return Err("Supply a specific credential and service/host in --title, and explain why it is needed and where to find it in --label (or --message); generic credential prompts are unsafe.".into());
    }
    Ok(value.trim().to_owned())
}

impl Options {
    fn context_text(&self) -> String {
        let hint = self
            .hint
            .as_ref()
            .map_or_else(String::new, |hint| format!("\nExpected format: {hint}"));
        format!("{}{hint}\n{LOGIN_WARNING}", self.label)
    }

    fn prompt_text(&self) -> String {
        format!("{}\n{}", self.title, self.context_text())
    }
}

fn parse_pattern(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::Regex::new(pattern)?;
    regex::Regex::new(&format!("\\A(?:{pattern})\\z")).or_else(|_| {
        // A trailing free-spacing comment can consume the closing group.
        regex::Regex::new(&format!("\\A(?:{pattern}\n)\\z"))
    })
}

/// Self-contained dispatch: no harness, daemon, transcript or model call.
pub fn run(options: &Options) -> i32 {
    if options.internal_vault_stdin {
        return report(sink::vault_from_stdin(options));
    }
    if options.dry_run {
        println!(
            "{}\n{}",
            options.prompt_text(),
            gui::destination_label(options)
        );
        return 0;
    }
    if !test_mode() || std::env::var_os("CLUD_TEST_LOKR_BINARY").is_some() {
        return report(lokr::request(options));
    }
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
                        "lokr is unavailable or could not prompt. Install forge-ci/lokr and put lokr on PATH (soldr cargo install --git https://github.com/forge-ci/lokr lokr). Tests require a fake lokr or fixture.",
                    Failure::Pattern => "The credential does not match the expected format. Request a new prompt explicitly.",
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
    let secret = validate_secret(prompt(options)?, options.multiline)?;
    if options
        .pattern
        .as_ref()
        .is_some_and(|pattern| !pattern.is_match(&secret))
    {
        return Err(Failure::Pattern);
    }
    Ok(secret)
}

fn acquire(options: &Options) -> Result<Secret, Failure> {
    if test_mode() {
        return fixture_prompt(
            std::env::var_os("CLUD_TEST_SECRET_PROMPT_CANCEL").is_some(),
            std::env::var_os("CLUD_TEST_SECRET_PROMPT_FILE")
                .as_deref()
                .map(std::path::Path::new),
        );
    }
    gui::prompt(options)
}

fn fixture_prompt(cancel: bool, path: Option<&std::path::Path>) -> Result<Secret, Failure> {
    if cancel {
        return Err(Failure::Cancelled);
    }
    let path = path.ok_or(Failure::Unavailable)?;
    std::fs::read_to_string(path)
        .map(Zeroizing::new)
        .map_err(|_| Failure::Unavailable)
}

fn real_ui_disabled() -> bool {
    cfg!(test)
        || std::env::var_os("CLUD_TEST_NO_SECRET_UI").is_some()
        || std::env::var_os("CLUD_INTEGRATION_TESTS").is_some()
        || std::env::var_os("PYTEST_CURRENT_TEST").is_some()
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
        internal_vault_stdin: false,
        title: "Git/SSH/sudo authentication credential".to_string(),
        label: argv.get(1).map_or_else(
            || "Enter the requested credential".to_string(),
            |value| value.to_string_lossy().into_owned(),
        ),
        hint: None,
        dry_run: false,
        multiline: false,
        pattern: None,
        to_file: None,
        to_keyring: Vec::new(),
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
    fn absent_or_cancelled_fixtures_never_fall_through_to_ui() {
        assert!(matches!(
            fixture_prompt(false, None),
            Err(Failure::Unavailable)
        ));
        assert!(matches!(
            fixture_prompt(true, None),
            Err(Failure::Cancelled)
        ));
    }

    #[test]
    fn pattern_mismatch_opens_only_one_prompt() {
        let options = Options::try_parse_from([
            "secret",
            "--title",
            "GitHub token for fixture.example",
            "--message",
            "Authenticate the fixture; find tokens in GitHub developer settings",
            "--pattern",
            "[0-9]+",
            "--to-file",
            "unused",
        ])
        .unwrap();
        let mut attempts = 0;
        assert!(matches!(
            acquire_with(&options, |_| {
                attempts += 1;
                Ok(Zeroizing::new("synthetic-mismatch".into()))
            }),
            Err(Failure::Pattern)
        ));
        assert_eq!(attempts, 1, "one invocation must not reopen the prompt");
    }

    #[test]
    fn dry_run_builds_request_without_acquisition_or_sink() {
        let options = Options::try_parse_from([
            "secret",
            "--title",
            "GitHub OAuth for fixture.example",
            "--message",
            "Enable login; find Client ID in GitHub developer settings",
            "--hint",
            "Iv1. prefix",
            "--dry-run",
            "--to-file",
            "/never/write/here",
        ])
        .unwrap();
        assert_eq!(run(&options), 0);
    }

    #[test]
    fn request_text_includes_context_hint_and_login_warning() {
        let options = Options::try_parse_from([
            "secret",
            "--title",
            "GitHub OAuth Client ID for datalake.kumquat.link",
            "--message",
            "Enable OAuth login; copy Client ID from GitHub developer settings",
            "--hint",
            "Iv1. followed by hexadecimal characters",
            "--to-file",
            "unused",
        ])
        .unwrap();
        assert_eq!(options.prompt_text(), "GitHub OAuth Client ID for datalake.kumquat.link\nEnable OAuth login; copy Client ID from GitHub developer settings\nExpected format: Iv1. followed by hexadecimal characters\nThis is NOT your computer or login password.");
        let mut without_hint = options;
        without_hint.hint = None;
        assert!(!without_hint.prompt_text().contains("Expected format:"));
    }

    #[test]
    fn request_context_is_required_and_specific() {
        let base = ["secret", "--to-file", "unused"];
        assert!(Options::try_parse_from(base).is_err());
        for flag in ["--title", "--label"] {
            assert!(Options::try_parse_from(
                base.into_iter()
                    .chain([flag, "GitHub OAuth for example.com"])
            )
            .is_err());
        }
        for generic in [
            "",
            "  ",
            "password",
            "SECRET",
            "credential",
            "Enter the requested credential",
            "clud credential request",
            "Please enter your password.",
            "Enter your password to continue",
            "Enter the requested credential below",
            "Password prompt",
        ] {
            for flag in ["--title", "--label"] {
                let other = if flag == "--title" {
                    "--label"
                } else {
                    "--title"
                };
                assert!(
                    Options::try_parse_from(base.into_iter().chain([
                        flag,
                        generic,
                        other,
                        "GitHub OAuth for example.com"
                    ]))
                    .is_err(),
                    "accepted {flag}={generic:?}"
                );
            }
        }
    }

    #[test]
    fn secret_pattern_rejects_before_storing() {
        let options = Options::try_parse_from([
            "secret",
            "--title",
            "GitHub API key for fixture.example",
            "--message",
            "Authenticate the fixture; find the key in GitHub developer settings",
            "--pattern",
            "[0-9]+",
            "--to-file",
            "unused",
        ])
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
    fn secret_pattern_accepts_and_delivers_once() {
        let options = Options::try_parse_from([
            "secret",
            "--title",
            "GitHub token for fixture.example",
            "--message",
            "Authenticate the fixture; find tokens in GitHub developer settings",
            "--pattern",
            "[0-9]+",
            "--to-file",
            "unused",
        ])
        .unwrap();
        let mut attempts = 0;
        let mut stored = 0;
        let result = acquire_with(&options, |_| {
            attempts += 1;
            Ok(Zeroizing::new("123".into()))
        })
        .map(|_| stored += 1);
        assert!(result.is_ok());
        assert_eq!(attempts, 1);
        assert_eq!(stored, 1);
    }

    #[test]
    fn secret_pattern_exhaustion_and_cancellation_never_deliver() {
        let options = Options::try_parse_from([
            "secret",
            "--title",
            "GitHub API key for fixture.example",
            "--message",
            "Authenticate the fixture; find the key in GitHub developer settings",
            "--pattern",
            "[0-9]+",
            "--to-file",
            "unused",
        ])
        .unwrap();
        let mut attempts = 0;
        let result = acquire_with(&options, |_| {
            attempts += 1;
            Ok(Zeroizing::new("synthetic-mismatch".into()))
        });
        assert!(matches!(result, Err(Failure::Pattern)));
        assert_eq!(attempts, 1);
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
        let error = Options::try_parse_from([
            "secret",
            "--title",
            "GitHub API key for fixture.example",
            "--message",
            "Authenticate the fixture; find the key in GitHub developer settings",
            "--pattern",
            "[",
            "--to-file",
            "unused",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn secret_pattern_absent_preserves_single_attempt_behavior() {
        let options = Options::try_parse_from([
            "secret",
            "--title",
            "GitHub API key for fixture.example",
            "--message",
            "Authenticate the fixture; find the key in GitHub developer settings",
            "--to-file",
            "unused",
        ])
        .unwrap();
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
        assert!(Options::try_parse_from([
            "secret",
            "--title",
            "GitHub token for fixture.example",
            "--message",
            "Authenticate the fixture; find tokens in GitHub developer settings",
        ])
        .is_err());
        assert!(Options::try_parse_from([
            "secret",
            "--title",
            "GitHub API key for fixture.example",
            "--message",
            "Authenticate the fixture; find the key in GitHub developer settings",
            "--to-file",
            "x",
            "--vault",
            "kimi"
        ])
        .is_err());
        let parsed = Options::try_parse_from([
            "secret",
            "--title",
            "GitHub API key for fixture.example",
            "--message",
            "Authenticate the fixture; find the key in GitHub developer settings",
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

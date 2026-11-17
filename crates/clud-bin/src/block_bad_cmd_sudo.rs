//! Refuse a raw `sudo` that would wait for a password the agent cannot type.
//!
//! An agent's shell call has no terminal, so `sudo cmd` fails with
//! `a terminal is required` / `a password is required`, and the agent then
//! asks for the password in chat. `clud sudo -- cmd` is the one blessed path:
//! it opens lokr's masked prompt only if sudo needs a password and runs the
//! command in the same call. `sudo -n`, `-k`, `-K`, `-V` and `--help` never
//! prompt and stay allowed. See `docs/architecture/secret-catalog.md#sudo`.
//!
//! Host terminals and clud's remote-connect leader never pass through this
//! hook; it only judges an agent's own tool calls.

use super::{accepted_override_reason, override_hint, program_name, CommandEvaluation};

pub(super) const SUDO_PROMPT_RULE_ID: &str = "sudo-prompt";

/// What one `sudo` invocation would do about a password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Never prompts (`-n`, `-k`/`-K` alone, `-V`, `--help`).
    Allow,
    /// Would prompt on a terminal the agent does not have.
    Prompts,
    /// `-S`: the password would come from the agent's own stdin.
    Stdin,
    /// `-A`: SUDO_ASKPASS, which sudo-rs rejects.
    Askpass,
}

const VALUE_SHORT: &[char] = &['u', 'g', 'h', 'p', 'C', 'T', 'R', 'D', 'U', 'r', 't'];
const VALUE_LONG: &[&str] = &[
    "--user",
    "--group",
    "--host",
    "--prompt",
    "--close-from",
    "--chroot",
    "--directory",
    "--command-timeout",
    "--role",
    "--type",
    "--other-user",
];

#[derive(Default)]
struct Flags {
    non_interactive: bool,
    stdin: bool,
    askpass: bool,
    reset: bool,
    remove: bool,
    info: bool,
    /// sudo options to keep in the replacement (`-u postgres`, `-E`, ...).
    kept: Vec<String>,
}

impl Flags {
    fn long(&mut self, word: &str, next: Option<&String>) -> usize {
        let name = word.split('=').next().unwrap_or(word);
        match name {
            "--non-interactive" => self.non_interactive = true,
            "--stdin" => self.stdin = true,
            "--askpass" => self.askpass = true,
            "--reset-timestamp" => self.reset = true,
            "--remove-timestamp" => self.remove = true,
            "--version" | "--help" => self.info = true,
            "--prompt" => return if word.contains('=') { 1 } else { 2 },
            _ if VALUE_LONG.contains(&name) && !word.contains('=') => {
                self.kept.push(word.to_owned());
                self.kept.extend(next.cloned());
                return 2;
            }
            _ => self.kept.push(word.to_owned()),
        }
        1
    }

    fn short(&mut self, word: &str, next: Option<&String>) -> usize {
        let mut kept = String::from("-");
        for (offset, flag) in word.char_indices().skip(1) {
            match flag {
                'n' => self.non_interactive = true,
                'S' => self.stdin = true,
                'A' => self.askpass = true,
                'k' => self.reset = true,
                'K' => self.remove = true,
                'V' => self.info = true,
                'v' | 'l' | 'e' => {}
                _ if VALUE_SHORT.contains(&flag) => {
                    let attached = &word[offset + flag.len_utf8()..];
                    if flag == 'h' && attached.is_empty() && next.is_none() {
                        self.info = true;
                        return 1;
                    }
                    if flag != 'p' {
                        kept.push(flag);
                        self.kept.push(kept);
                        if attached.is_empty() {
                            self.kept.extend(next.cloned());
                        } else {
                            let last = self.kept.len() - 1;
                            self.kept[last].push_str(attached);
                        }
                    } else if kept.len() > 1 {
                        self.kept.push(kept);
                    }
                    return if attached.is_empty() { 2 } else { 1 };
                }
                other => kept.push(other),
            }
        }
        if kept.len() > 1 {
            self.kept.push(kept);
        }
        1
    }
}

/// Parse `sudo [options] [command]`; returns the flags and the command index.
fn parse(words: &[String]) -> (Flags, usize) {
    let mut flags = Flags::default();
    let mut index = 1;
    while let Some(word) = words.get(index) {
        if word == "--" {
            index += 1;
            break;
        }
        if !word.starts_with('-') || word == "-" {
            break;
        }
        let next = words.get(index + 1);
        index += if word.starts_with("--") {
            flags.long(word, next)
        } else {
            flags.short(word, next)
        };
    }
    (flags, index.min(words.len()))
}

/// The decision table for one invocation whose program is `sudo`.
pub(super) fn classify(words: &[String]) -> Verdict {
    let (flags, command) = parse(words);
    let has_command = command < words.len();
    if flags.stdin {
        Verdict::Stdin
    } else if flags.askpass {
        Verdict::Askpass
    } else if flags.non_interactive
        || flags.info
        || flags.remove
        || (flags.reset && !has_command && !words.iter().any(|w| is_validate_or_list(w)))
    {
        Verdict::Allow
    } else {
        Verdict::Prompts
    }
}

fn is_validate_or_list(word: &str) -> bool {
    matches!(word, "--validate" | "--list")
        || (word.starts_with('-')
            && !word.starts_with("--")
            && word.chars().skip(1).any(|flag| matches!(flag, 'v' | 'l')))
}

/// The blessed spelling of this invocation.
pub(super) fn replacement(words: &[String]) -> String {
    let (flags, command) = parse(words);
    let mut argv = flags.kept;
    argv.extend(words[command..].iter().cloned());
    if words.len() <= command {
        return "clud sudo -- <command> [args...]".into();
    }
    format!("clud sudo -- {}", shell_words::join(&argv))
}

/// The hook's per-command rule: `Some(reason)` refuses the call.
pub(super) fn reason(words: &[String], evaluation: &mut CommandEvaluation) -> Option<String> {
    // Windows `sudo` elevates through UAC on the user's desktop.
    if crate::platform::host::WINDOWS || program_name(words.first()?) != "sudo" {
        return None;
    }
    let verdict = classify(words);
    if verdict == Verdict::Allow {
        return None;
    }
    if let Some(override_reason) = accepted_override_reason(SUDO_PROMPT_RULE_ID) {
        evaluation.log_messages.push(format!(
            "BAD_CMD_OVERRIDE accepted rule={SUDO_PROMPT_RULE_ID} reason={override_reason:?}"
        ));
        return None;
    }
    Some(message(verdict, words))
}

pub(super) fn message(verdict: Verdict, words: &[String]) -> String {
    let why = match verdict {
        Verdict::Stdin => "`sudo -S` reads the password from this command's stdin, which puts it in the transcript.",
        Verdict::Askpass => "`sudo -A` depends on SUDO_ASKPASS, and sudo-rs (Ubuntu 25.10+) rejects `-A`.",
        Verdict::Prompts | Verdict::Allow => "this sudo would wait for a password on a terminal this agent does not have (`sudo: a terminal is required` / `a password is required`), and a ticket from one tool call never reaches the next.",
    };
    format!(
        "[sudo-prompt] {why} Run `{}` instead: it asks the user through clud's masked password prompt only if sudo needs one and runs the command in this same call. Never ask for the password in chat or tell the user to run the command themselves. `sudo -n ...` stays allowed for passwordless sudo. {}",
        replacement(words),
        override_hint(SUDO_PROMPT_RULE_ID)
    )
}

#[cfg(test)]
#[path = "block_bad_cmd_sudo_tests.rs"]
mod tests;

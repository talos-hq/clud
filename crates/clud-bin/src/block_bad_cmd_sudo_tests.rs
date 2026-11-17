//! Decision table for the `sudo-prompt` rule, plus the whole-hook path.

use super::super::tests::temp_env;
use super::super::{evaluate_command, BAD_CMD_OVERRIDE_ENV};
use super::*;

fn words(command: &str) -> Vec<String> {
    shell_words::split(command).unwrap()
}

#[test]
fn decision_table() {
    for (command, expected) in [
        // Prompts on a terminal the agent does not have.
        ("sudo apt-get install -y foo", Verdict::Prompts),
        ("sudo -u postgres psql", Verdict::Prompts),
        ("sudo -E make install", Verdict::Prompts),
        ("sudo -v", Verdict::Prompts),
        ("sudo -l", Verdict::Prompts),
        ("sudo -k apt-get update", Verdict::Prompts),
        ("sudo -kv", Verdict::Prompts),
        ("sudo -i", Verdict::Prompts),
        ("sudo -s whoami", Verdict::Prompts),
        ("sudo -h otherhost id", Verdict::Prompts),
        ("sudo -p 'pw:' id", Verdict::Prompts),
        ("/usr/bin/sudo id", Verdict::Prompts),
        ("sudo DEBIAN_FRONTEND=noninteractive apt-get install -y foo", Verdict::Prompts),
        // The password would come from the agent's stdin.
        ("sudo -S id", Verdict::Stdin),
        ("sudo --stdin id", Verdict::Stdin),
        ("sudo -nS id", Verdict::Stdin),
        ("sudo -S -p '' -v", Verdict::Stdin),
        // SUDO_ASKPASS: rejected by sudo-rs.
        ("sudo -A id", Verdict::Askpass),
        ("sudo --askpass apt-get update", Verdict::Askpass),
        // Never prompts.
        ("sudo -n true", Verdict::Allow),
        ("sudo -n -v", Verdict::Allow),
        ("sudo --non-interactive apt-get update", Verdict::Allow),
        ("sudo -nu postgres psql", Verdict::Allow),
        ("sudo -u root -n id", Verdict::Allow),
        ("sudo -k", Verdict::Allow),
        ("sudo -K", Verdict::Allow),
        ("sudo --reset-timestamp", Verdict::Allow),
        ("sudo -V", Verdict::Allow),
        ("sudo --version", Verdict::Allow),
        ("sudo -h", Verdict::Allow),
        ("sudo --help", Verdict::Allow),
    ] {
        assert_eq!(classify(&words(command)), expected, "{command}");
    }
}

#[test]
fn replacement_keeps_target_options_and_drops_password_plumbing() {
    for (command, expected) in [
        ("sudo apt-get install -y foo", "clud sudo -- apt-get install -y foo"),
        ("sudo -u postgres psql -c 'select 1'", "clud sudo -- -u postgres psql -c 'select 1'"),
        ("sudo -upostgres psql", "clud sudo -- -upostgres psql"),
        ("sudo -E -H make install", "clud sudo -- -E -H make install"),
        ("sudo -S -p '' id -u", "clud sudo -- id -u"),
        ("sudo -A apt-get update", "clud sudo -- apt-get update"),
        ("sudo -Ek id", "clud sudo -- -E id"),
        ("sudo --prompt=x id", "clud sudo -- id"),
        ("sudo -v", "clud sudo -- <command> [args...]"),
        ("sudo -- id -u", "clud sudo -- id -u"),
    ] {
        assert_eq!(replacement(&words(command)), expected, "{command}");
    }
}

fn hook_reason(command: &str) -> Option<String> {
    let mut reason = None;
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        reason = evaluate_command(command, None, false, &[]).reason;
    });
    reason
}

#[test]
fn hook_refuses_prompting_sudo_with_the_exact_blessed_command() {
    if crate::platform::host::WINDOWS {
        return;
    }
    for (command, blessed) in [
        ("sudo apt-get install -y foo", "`clud sudo -- apt-get install -y foo`"),
        ("cd /tmp && sudo make install", "`clud sudo -- make install`"),
        ("echo pw | sudo -S id", "`clud sudo -- id`"),
        ("SUDO_ASKPASS=/x sudo -A id", "`clud sudo -- id`"),
        ("bash -c 'sudo systemctl restart nginx'", "`clud sudo -- systemctl restart nginx`"),
        ("env FOO=1 sudo id", "`clud sudo -- id`"),
        ("x=$(sudo cat /etc/shadow)", "`clud sudo -- cat /etc/shadow`"),
    ] {
        let reason = hook_reason(command).unwrap_or_else(|| panic!("{command} should be refused"));
        assert!(reason.starts_with("[sudo-prompt]"), "{command}: {reason}");
        assert!(reason.contains(blessed), "{command}: {reason}");
        assert!(reason.contains("masked password prompt"), "{reason}");
        assert!(reason.contains("Never ask for the password in chat"), "{reason}");
        assert!(reason.contains("CLUD_BAD_CMD_OVERRIDE=\"sudo-prompt:"), "{reason}");
    }
}

#[test]
fn hook_allows_non_prompting_sudo_and_the_blessed_forms() {
    for command in [
        "sudo -n true",
        "sudo -n apt-get update",
        "sudo -k",
        "sudo -K",
        "sudo -V",
        "clud sudo -- apt-get install -y foo",
        "clud sudo --reason 'Install foo' -- apt-get install -y foo",
        "clud sudo -- -u postgres psql",
        "clud secret --title 'sudo password for alice on host' --message 'Validate sudo' --to-command sudo -S -p '' -v",
        "echo sudo is great",
        "grep -r sudo /etc/sudoers.d",
        "man sudo",
    ] {
        assert_eq!(hook_reason(command), None, "{command}");
    }
}

#[test]
fn override_lets_one_raw_sudo_through() {
    if crate::platform::host::WINDOWS {
        return;
    }
    let mut reason = Some(String::new());
    temp_env(
        BAD_CMD_OVERRIDE_ENV,
        "sudo-prompt:host has a real tty for this call",
        || {
            reason = evaluate_command("sudo apt-get update", None, false, &[]).reason;
        },
    );
    assert_eq!(reason, None);
}

//! `.claude/worktrees/` is not a place for work (#1962, #1971): nothing may
//! create it or put new entries in it, in the repo or the home directory.

use super::block_bad_cmd_claude_worktrees::{reason, RULE_ID};
use super::tests::temp_env;
use super::*;
use serde_json::json;

fn bash(command: &str, cwd: &str) -> Option<String> {
    reason("Bash", None, command, ShellDialect::Posix, Path::new(cwd))
}

fn tool(tool_name: &str, input: Value) -> Option<String> {
    reason(
        tool_name,
        Some(&input),
        "",
        ShellDialect::Posix,
        Path::new("/repo"),
    )
}

fn denied(verdict: Option<String>, what: &str) {
    let reason = verdict.unwrap_or_else(|| panic!("{what} should be denied"));
    assert!(reason.contains(RULE_ID), "{what}: {reason}");
    assert!(
        reason.contains("~/.clud/worktrees"),
        "names the sanctioned place: {reason}"
    );
}

#[test]
fn shell_commands_that_create_entries_there_are_denied() {
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        for command in [
            "mkdir -p .claude/worktrees/scratch",
            "mkdir .claude/worktrees",
            "echo notes > .claude/worktrees/bridge-61-handoff.md",
            "cat plan.md >> /repo/.claude/worktrees/plan.md",
            "cd .claude/worktrees && touch handoff.md",
            "(cd /repo/.claude && mkdir worktrees)",
            "tee .claude/worktrees/log.txt < out",
            "cp plan.md .claude/worktrees/",
            "mv draft.md /repo/.claude/worktrees/draft.md",
            "cp -t .claude/worktrees a b",
            "git worktree add .claude/worktrees/feat -b feat origin/main",
            "git -C /repo worktree add .claude/worktrees/feat",
            "D=.claude/worktrees; mkdir -p \"$D/x\"",
            "mkdir -p ~/.claude/worktrees/x",
            "bash -c 'touch .claude/worktrees/x'",
        ] {
            denied(bash(command, "/repo"), command);
        }
    });
}

#[test]
fn powershell_and_cmd_spellings_are_denied() {
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        for command in [
            r"New-Item -ItemType Directory .claude\worktrees\x",
            r"Set-Content -Path .claude\worktrees\notes.md -Value hi",
            r"echo hi | Out-File C:\repo\.claude\worktrees\notes.md",
        ] {
            denied(
                reason(
                    "PowerShell",
                    None,
                    command,
                    ShellDialect::PowerShell,
                    Path::new(r"C:\repo"),
                ),
                command,
            );
        }
        let command = r"mkdir .claude\worktrees\x";
        denied(
            reason(
                "cmd",
                None,
                command,
                ShellDialect::Cmd,
                Path::new(r"C:\repo"),
            ),
            command,
        );
    });
}

#[test]
fn file_tools_writing_there_are_denied() {
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        for (name, input) in [
            (
                "Write",
                json!({"file_path": "/repo/.claude/worktrees/bridge-61-handoff.md"}),
            ),
            (
                "Edit",
                json!({"file_path": "/home/u/.claude/worktrees/notes.md"}),
            ),
            (
                "MultiEdit",
                json!({"file_path": "/repo/.claude/worktrees/x/a.rs"}),
            ),
            (
                "NotebookEdit",
                json!({"notebook_path": "/repo/.claude/worktrees/n.ipynb"}),
            ),
        ] {
            denied(tool(name, input), name);
        }
    });
}

/// Claude Code's own worktrees (`Agent` isolation, `EnterWorktree`) are not
/// refused: clud's `WorktreeCreate` hook builds them under `~/.clud/worktrees`
/// instead of `.claude/worktrees/`.
#[test]
fn claude_code_worktree_tools_are_allowed_because_they_are_relocated() {
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        assert_eq!(
            tool("Agent", json!({"prompt": "x", "isolation": "worktree"})),
            None
        );
        assert_eq!(tool("EnterWorktree", json!({"name": "feat"})), None);
    });
}

#[test]
fn reads_cleanup_and_other_places_are_allowed() {
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        for command in [
            "ls -la .claude/worktrees",
            "cat .claude/worktrees/bridge-61-handoff.md",
            "git worktree remove --force .claude/worktrees/agent-1",
            "safe-rm -r .claude/worktrees/scratch",
            "mv .claude/worktrees/feat /home/u/.clud/worktrees/repo-wt-feat",
            "git worktree add /home/u/.clud/worktrees/repo-wt-feat -b feat",
            "mkdir -p .claude/agents/x",
            "echo hi > notes/worktrees.md",
            "mkdir \"$UNKNOWN_DIR\"",
        ] {
            assert_eq!(bash(command, "/repo"), None, "{command}");
        }
        assert_eq!(
            tool("Write", json!({"file_path": "/repo/src/main.rs"})),
            None
        );
        assert_eq!(tool("Agent", json!({"prompt": "x"})), None);
        assert_eq!(
            tool("Read", json!({"file_path": "/repo/.claude/worktrees/a.md"})),
            None
        );
    });
}

#[test]
fn work_inside_an_existing_checkout_there_is_allowed() {
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().expect("tempdir");
        let checkout = root
            .path()
            .join(".claude")
            .join("worktrees")
            .join("agent-1");
        std::fs::create_dir_all(checkout.join("src")).expect("checkout dirs");
        std::fs::write(checkout.join(".git"), "gitdir: /elsewhere\n").expect(".git file");
        let file = checkout.join("src").join("lib.rs");
        let input = json!({"file_path": file.to_string_lossy()});
        assert_eq!(tool("Edit", input), None);
        let cwd = checkout.to_string_lossy().to_string();
        assert_eq!(
            bash("echo x > src/out.txt && mkdir -p target/tmp", &cwd),
            None
        );
        // A loose entry beside the checkout is still refused.
        let loose = root
            .path()
            .join(".claude")
            .join("worktrees")
            .join("notes.md");
        denied(
            tool("Write", json!({"file_path": loose.to_string_lossy()})),
            "loose file next to a checkout",
        );
    });
}

#[test]
fn the_override_allows_one_call() {
    temp_env(
        BAD_CMD_OVERRIDE_ENV,
        "claude-worktrees-dir:migrating an old checkout",
        || {
            assert_eq!(bash("mkdir -p .claude/worktrees/x", "/repo"), None);
            assert_eq!(
                tool(
                    "Write",
                    json!({"file_path": "/repo/.claude/worktrees/x.md"})
                ),
                None
            );
        },
    );
}

#[test]
fn a_worktree_add_there_is_handed_the_command_rewritten_into_the_worktree_root() {
    let root = Path::new("/home/u/.clud/worktrees");
    for (command, rewritten) in [
        (
            "git worktree add .claude/worktrees/feat -b feat origin/main",
            "git worktree add /home/u/.clud/worktrees/repo-wt-feat -b feat origin/main",
        ),
        (
            "git worktree add /repo/.claude/worktrees/agent-7",
            "git worktree add /home/u/.clud/worktrees/repo-wt-agent-7 -b agent-7",
        ),
        // The home directory's `.claude` belongs to no repo; the repo is
        // the one the command runs in.
        (
            "git worktree add ~/.claude/worktrees/x -b x",
            "git worktree add /home/u/.clud/worktrees/repo-wt-x -b x",
        ),
    ] {
        let mut verdict = None;
        temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
            verdict = super::block_bad_cmd_claude_worktrees::reason_in(
                "Bash",
                None,
                command,
                ShellDialect::Posix,
                Path::new("/repo"),
                Some(root),
            );
        });
        let reason = verdict.unwrap_or_else(|| panic!("{command} should be denied"));
        assert!(
            reason.contains(&format!("`{rewritten}`")),
            "{command}: expected `{rewritten}` in: {reason}"
        );
    }
}

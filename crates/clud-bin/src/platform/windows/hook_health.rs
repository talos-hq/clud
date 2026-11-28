//! Windows-only tests of `crate::hook_health`: Claude's Windows stdin bug and
//! Codex batch-hook exit codes.

#[cfg(test)]
mod tests {
    use crate::hook_health::tests::{trusted_state_for, write};
    use crate::hook_health::{apply_deterministic_repairs, deterministic_repair_actions};
    use crate::hook_health::{codex_project_key, inspect_paths, plan_repairs, RepairAction};
    use kernal_api::platform::fs::TemporaryDirectory;
    use std::fs;

    #[test]
    fn claude_windows_hook_stdin_bug_warning_mentions_workaround() {
        let temp = TemporaryDirectory::new().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        write(
            &repo.join(".claude").join("settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"python check.py"}]}]}}"#,
        );

        let report = inspect_paths(&repo, Some(&home));

        let warning = report
            .warnings
            .iter()
            .find(|warning| warning.contains("github.com/anthropics/claude-code/issues/53177"))
            .expect("Claude Windows hooks should report the upstream stdin bug");
        assert!(warning.contains("hook timeout"), "{warning}");
        assert!(warning.contains("policy denial"), "{warning}");
        assert!(warning.contains("CLAUDE_CODE_GIT_BASH_PATH"), "{warning}");
        assert!(warning.contains(r"bin\bash.exe"), "{warning}");
        assert!(warning.contains("git-bash.exe"), "{warning}");
        assert!(warning.contains("where bash"), "{warning}");
    }

    #[test]
    fn codex_batch_hook_exit_code_risk_warns_and_plans_repair() {
        let temp = TemporaryDirectory::new().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        let codex = repo.join(".codex").join("hooks.json");
        write(
            &codex,
            r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"C:\\tools\\guard.cmd --check"}]}]}}"#,
        );
        write(
            &home.join(".codex").join("config.toml"),
            &format!(
                "[projects.'{}']\ntrust_level = \"trusted\"\n{}",
                codex_project_key(&repo),
                trusted_state_for(&codex, 0, 0)
            ),
        );

        let report = inspect_paths(&repo, Some(&home));

        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("may fail open")));
        assert!(plan_repairs(&report).into_iter().any(|action| matches!(
            action,
            RepairAction::NormalizeCodexBatchHookExitCode { hooks_path } if hooks_path == codex
        )));
    }

    #[test]
    fn codex_batch_hook_exit_code_repair_appends_lastexitcode() {
        let temp = TemporaryDirectory::new().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        let codex = repo.join(".codex").join("hooks.json");
        write(
            &codex,
            r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"C:\\tools\\guard.cmd --check"},{"type":"command","command":"C:\\tools\\safe.cmd; exit $LASTEXITCODE"}]}]}}"#,
        );
        write(
            &home.join(".codex").join("config.toml"),
            &format!(
                "[projects.'{}']\ntrust_level = \"trusted\"\n{}",
                codex_project_key(&repo),
                trusted_state_for(&codex, 0, 0)
            ),
        );

        let report = inspect_paths(&repo, Some(&home));
        apply_deterministic_repairs(deterministic_repair_actions(&report)).unwrap();

        let text = fs::read_to_string(codex).unwrap();
        assert!(
            text.contains(r#"C:\\tools\\guard.cmd --check; exit $LASTEXITCODE"#),
            "{text}"
        );
        assert_eq!(text.matches("safe.cmd; exit $LASTEXITCODE").count(), 1);
    }
}

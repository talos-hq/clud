//! #1962: the scanner reads evaluated syntax trees, so git clone and
//! worktree-add destinations are found however the command is spelled, and
//! the extern-directory clone guard cannot be sidestepped with `git -C`.

use super::tests::temp_env;
use super::*;

/// Evaluate with an explicit worktree root; `None` turns the worktree
/// location guard off so capture can be tested on its own.
fn evaluate_with(
    command: &str,
    cwd: &str,
    repo_root: Option<&str>,
    worktree_root: Option<&str>,
) -> CommandEvaluation {
    let cwd = PathBuf::from(cwd);
    let context = EvaluationContext {
        cwd: Some(&cwd),
        allow_hybrid_uv_run: false,
        bad_commands: &[],
        bad_pipelines: &[],
        repo_root: repo_root.map(Path::new),
        worktree_root: worktree_root.map(Path::new),
        pr_wait_fail_fast_enabled: true,
        rust_use_soldr: true,
    };
    let mut evaluation = CommandEvaluation::default();
    evaluate_command_into(command, &context, ShellDialect::Posix, 0, &mut evaluation);
    evaluation
}

fn evaluate(command: &str, cwd: &str, repo_root: Option<&str>) -> CommandEvaluation {
    evaluate_with(command, cwd, repo_root, None)
}

fn captured(command: &str) -> Vec<(&'static str, PathBuf)> {
    let evaluation = evaluate(command, "/w", None);
    assert!(
        evaluation.reason.is_none(),
        "{command}: unexpected denial {:?}",
        evaluation.reason
    );
    evaluation
        .git_path_captures
        .iter()
        .map(|capture| (capture.kind, capture.path.clone()))
        .collect()
}

fn worktree(path: &str) -> (&'static str, PathBuf) {
    (GIT_WORKTREE_ADD_CAPTURE_KIND, PathBuf::from(path))
}

fn clone(path: &str) -> (&'static str, PathBuf) {
    (GIT_CLONE_CAPTURE_KIND, PathBuf::from(path))
}

#[test]
fn every_shell_spelling_of_worktree_add_is_captured() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("git worktree add /x/v01 -b v01", &[("w", "/x/v01")]),
        ("git -C /repo worktree add ../v02 -b v02", &[("w", "/v02")]),
        (
            "(cd /repo && git worktree add ../v03 -b v03 2>&1 | tail -1)",
            &[("w", "/v03")],
        ),
        (
            "(cd /repo && git -c core.quotepath=off worktree add ../v04 -b v04)",
            &[("w", "/v04")],
        ),
        (
            "(cd /repo && /run/current-system/sw/bin/git worktree add ../v06 -b v06)",
            &[("w", "/v06")],
        ),
        (
            "(cd /repo && for n in 07a 07b; do git worktree add ../v$n-loop -b v$n; done)",
            &[("w", "/v07a-loop"), ("w", "/v07b-loop")],
        ),
        (
            "(cd /repo && bash -c 'git worktree add ../v08 -b v08')",
            &[("w", "/v08")],
        ),
        (
            "G=git; W=worktree; $G -C /repo $W add ../v10 -b v10",
            &[("w", "/v10")],
        ),
        (
            "git --git-dir=/repo/.git worktree add /x/v11 -b v11",
            &[("w", "/x/v11")],
        ),
        (
            "git -C /x clone -q --bare repo v13.git",
            &[("c", "/x/v13.git")],
        ),
        (
            "cp -a /repo /x/v15 && git -C /x/v15 worktree add ../v15b -b v15",
            &[("w", "/x/v15b")],
        ),
        (
            "env -i /usr/bin/git clone https://h/r /x/xx",
            &[("c", "/x/xx")],
        ),
        ("cd /repo && git clone https://h/r.git", &[("c", "/repo/r")]),
    ];
    for (command, expected) in cases {
        let expected: Vec<_> = expected
            .iter()
            .map(|(kind, path)| {
                if *kind == "w" {
                    worktree(path)
                } else {
                    clone(path)
                }
            })
            .collect();
        assert_eq!(captured(command), expected, "{command}");
    }
}

#[test]
fn heredoc_text_and_other_git_commands_are_not_captured() {
    for command in [
        "cat <<EOF\ngit worktree add /not/real\nEOF",
        "echo git clone https://h/r x",
        "git -C /repo worktree list",
        "git init -q /x/v16",
        "git -C /repo status",
    ] {
        assert!(captured(command).is_empty(), "{command}");
    }
}

#[test]
fn every_spelling_of_a_clone_outside_the_extern_directory_is_denied() {
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        for command in [
            "git clone https://example.com/foo.git ../scratch/foo",
            "git -C /elsewhere clone https://example.com/foo.git foo",
            "git -c core.x=y clone https://example.com/foo.git ../scratch/foo",
            "cd /tmp && git clone https://example.com/foo.git foo",
            "G=git; $G clone https://example.com/foo.git ../scratch/foo",
            "bash -c 'git clone https://example.com/foo.git ../scratch/foo'",
            "/usr/bin/git clone https://example.com/foo.git ../scratch/foo",
        ] {
            let evaluation = evaluate(command, "/repo", Some("/repo"));
            let reason = evaluation
                .reason
                .unwrap_or_else(|| panic!("`{command}` should be denied"));
            assert!(
                reason.contains(CLONE_EXTERN_REPOS_GUARD_RULE_ID),
                "{reason}"
            );
            assert!(evaluation.git_path_captures.is_empty(), "{command}");
        }
    });
}

#[test]
fn clones_landing_in_the_extern_directory_are_allowed_however_spelled() {
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        for command in [
            "git clone https://example.com/foo.git ../repo-extern/foo",
            "git -C /repo-extern clone https://example.com/foo.git",
            "cd ../repo-extern && git clone https://example.com/foo.git",
        ] {
            let evaluation = evaluate(command, "/repo", Some("/repo"));
            assert!(
                evaluation.reason.is_none(),
                "`{command}`: {:?}",
                evaluation.reason
            );
            assert_eq!(
                evaluation
                    .git_path_captures
                    .iter()
                    .map(|capture| capture.path.clone())
                    .collect::<Vec<_>>(),
                [PathBuf::from("/repo-extern/foo")],
                "{command}"
            );
        }
    });
}

// ---- Worktrees belong in clud's worktree root (`~/.clud/worktrees`), where
// the daemon reclaims them, or in /grind's own feature-worktree directory.

const WT_ROOT: &str = "/home/u/.clud/worktrees";

#[test]
fn worktree_add_outside_the_worktree_root_is_denied_however_spelled() {
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        for command in [
            "git worktree add .claude/worktrees/feat -b feat origin/main",
            "git worktree add ../repo-wt-feat -b feat",
            "git -C /repo worktree add /tmp/wt -b feat",
            "cd /repo && git worktree add ../elsewhere",
            "(cd /repo && git -c x=y worktree add .claude/worktrees/x)",
            "W=worktree; git $W add /scratch/wt",
            "bash -c 'git worktree add /scratch/wt'",
            "/usr/bin/git worktree add /scratch/wt",
        ] {
            let evaluation = evaluate_with(command, "/repo", Some("/repo"), Some(WT_ROOT));
            let reason = evaluation
                .reason
                .unwrap_or_else(|| panic!("`{command}` should be denied"));
            assert!(
                reason.contains(WORKTREE_ROOT_GUARD_RULE_ID),
                "{command}: {reason}"
            );
            assert!(
                reason.contains(WT_ROOT),
                "names the sanctioned root: {reason}"
            );
            assert!(evaluation.git_path_captures.is_empty(), "{command}");
        }
    });
}

#[test]
fn worktree_add_under_the_worktree_root_or_grind_dir_is_allowed() {
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        for (command, path) in [
            (
                "git worktree add /home/u/.clud/worktrees/clud-wt-1962 -b feat origin/main",
                "/home/u/.clud/worktrees/clud-wt-1962",
            ),
            (
                "git -C /repo worktree add ../home/u/.clud/worktrees/repo-wt-7",
                "/home/u/.clud/worktrees/repo-wt-7",
            ),
            (
                "git worktree add .clud/grind/worktrees/feature -b grind/meta-1 origin/main",
                "/repo/.clud/grind/worktrees/feature",
            ),
        ] {
            let evaluation = evaluate_with(command, "/repo", Some("/repo"), Some(WT_ROOT));
            assert!(
                evaluation.reason.is_none(),
                "`{command}`: {:?}",
                evaluation.reason
            );
            assert_eq!(
                evaluation.git_path_captures[0].path,
                PathBuf::from(path),
                "{command}"
            );
        }
    });
}

/// The refusal for `command` run from `cwd`, in repo `/repo`.
fn root_refusal(command: &str, cwd: &str, root: &str) -> String {
    let mut reason = None;
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        reason = evaluate_with(command, cwd, Some("/repo"), Some(root)).reason;
    });
    reason.unwrap_or_else(|| panic!("`{command}` should be denied"))
}

#[test]
fn the_refusal_hands_over_the_command_rewritten_into_the_worktree_root() {
    // Everything but the destination is kept as typed. Without a branch
    // argument git names the new branch after the folder, so the rewrite
    // keeps that name with `-b`.
    for (command, cwd, rewritten) in [
        (
            "git worktree add ../repo-wt-feat -b feat",
            "/repo",
            "git worktree add /home/u/.clud/worktrees/repo-wt-feat -b feat",
        ),
        (
            "git worktree add /tmp/wt -b feat origin/main",
            "/repo",
            "git worktree add /home/u/.clud/worktrees/repo-wt-wt -b feat origin/main",
        ),
        (
            "git worktree add ../elsewhere",
            "/repo",
            "git worktree add /home/u/.clud/worktrees/repo-wt-elsewhere -b elsewhere",
        ),
        (
            "git worktree add ../hotfix main",
            "/repo",
            "git worktree add /home/u/.clud/worktrees/repo-wt-hotfix main",
        ),
        (
            "git worktree add --detach /scratch/wt HEAD~1",
            "/repo",
            "git worktree add --detach /home/u/.clud/worktrees/repo-wt-wt HEAD~1",
        ),
        (
            "git -C /repo worktree add /tmp/wt -b feat",
            "/repo",
            "git -C /repo worktree add /home/u/.clud/worktrees/repo-wt-wt -b feat",
        ),
        (
            "git worktree add 'my wt' -b feat",
            "/repo",
            "git worktree add /home/u/.clud/worktrees/repo-wt-my-wt -b feat",
        ),
        // Run from elsewhere, the rewrite still runs in the repo.
        (
            "cd /repo && git worktree add ../x -b x",
            "/elsewhere",
            "git -C /repo worktree add /home/u/.clud/worktrees/repo-wt-x -b x",
        ),
    ] {
        let reason = root_refusal(command, cwd, WT_ROOT);
        assert!(
            reason.contains(&format!("`{rewritten}`")),
            "{command}: expected `{rewritten}` in: {reason}"
        );
    }
}

#[test]
fn an_added_branch_flag_comes_with_the_command_for_an_existing_branch() {
    let reason = root_refusal("git worktree add ../elsewhere", "/repo", WT_ROOT);
    assert!(
        reason.contains("`git worktree add /home/u/.clud/worktrees/repo-wt-elsewhere elsewhere`"),
        "{reason}"
    );
}

#[test]
fn the_rewritten_destination_skips_names_already_taken() {
    let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    std::fs::create_dir(root.path().join("repo-wt-feat")).unwrap();
    let root_text = crate::path_norm::display_slash(root.path());
    let reason = root_refusal("git worktree add /tmp/feat -b feat", "/repo", &root_text);
    assert!(
        reason.contains(&format!(
            "`git worktree add {root_text}/repo-wt-feat-2 -b feat`"
        )),
        "{reason}"
    );
    assert!(
        !root.path().join("repo-wt-feat-2").exists(),
        "the hook only names the path; it never creates it"
    );
}

#[test]
fn worktree_root_guard_honors_its_override() {
    temp_env(
        BAD_CMD_OVERRIDE_ENV,
        "git-worktree-add-outside-worktree-root:one-off experiment",
        || {
            let evaluation = evaluate_with(
                "git worktree add /scratch/wt",
                "/repo",
                Some("/repo"),
                Some(WT_ROOT),
            );
            assert!(evaluation.reason.is_none(), "{:?}", evaluation.reason);
            assert_eq!(evaluation.git_path_captures.len(), 1);
        },
    );
}

/// #2103 (from #1961): the clone guard's hint says the override must be the
/// hook's real environment, since a `VAR=value` prefix on the command never
/// reaches it — the inline spelling #1961's agent tried is still denied.
#[test]
fn clone_guard_hint_names_the_real_environment_not_an_inline_prefix() {
    temp_env(BAD_CMD_OVERRIDE_ENV, "unrelated-rule:reason", || {
        let command = "CLUD_BAD_CMD_OVERRIDE='git-clone-outside-extern-repos:x' \
                       git -C /elsewhere clone https://example.com/foo.git foo";
        let reason = evaluate(command, "/repo", Some("/repo"))
            .reason
            .expect("an inline prefix does not bypass the guard");
        assert!(reason.contains("not text prepended"), "{reason}");
    });
}

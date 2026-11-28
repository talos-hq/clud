//! Refuse `git diff @{upstream}...HEAD` when the branch's tracking ref is
//! stale (#1301).
//!
//! After a rebase or force-push the tracking ref still names the old branch
//! head, so a range against it swallows the whole rebase delta: a 4-file PR
//! read as 211 files and 13,488 lines, and the review burned a million
//! uncached tokens before the cache-health fuse stopped it. The built-in
//! `/code-review` hardcodes that range, and clud does not own its text, so the
//! hook is the one place to stop it. The denial names the correct range and
//! its diffstat, so the model retries once with the right one.
//!
//! "Stale" is exact: `HEAD` does not descend from the upstream commit.

use std::path::Path;

use super::block_bad_cmd_gate::statement_words;

/// The denial for a command that diffs against a stale upstream, or `None`.
pub(super) fn reason(command: &str, cwd: &Path) -> Option<String> {
    // Cheap pre-check: nearly every command lacks the spelling.
    let lower = command.to_ascii_lowercase();
    if !lower.contains("@{u") {
        return None;
    }
    let statements = statement_words(command).ok()?;
    let uses_upstream = statements.iter().any(|words| diffs_against_upstream(words));
    if !uses_upstream {
        return None;
    }
    let git = |args: &[&str]| crate::worktrees::run_git(cwd, args);
    let upstream = git(&["rev-parse", "--verify", "--quiet", "@{upstream}"]).ok()?;
    let upstream = upstream.trim();
    if upstream.is_empty() {
        return None;
    }
    // `is-ancestor` exits 0 when the upstream is an ancestor of HEAD.
    if git(&["merge-base", "--is-ancestor", upstream, "HEAD"]).is_ok() {
        return None;
    }
    Some(match correct_range(cwd) {
        Some((range, stat)) => format!(
            "`@{{upstream}}` is stale: HEAD does not descend from it (a rebase or force-push \
             moved the branch), so this range would include the whole rebase delta. Use \
             `git diff {range}` instead ({stat}), or run `\"$CLUD_EXE\" tool run \
             git/review_range.py` for the pinned SHAs (#1301)."
        ),
        None => "`@{upstream}` is stale: HEAD does not descend from it (a rebase or force-push \
                 moved the branch), so this range would include the whole rebase delta. Run \
                 `\"$CLUD_EXE\" tool run git/review_range.py` for the correct range (#1301)."
            .to_string(),
    })
}

/// `git [-C dir] diff ... @{upstream}/@{u} ...`.
fn diffs_against_upstream(words: &[String]) -> bool {
    let Some(program) = words.first() else {
        return false;
    };
    let bare = program.rsplit(['/', '\\']).next().unwrap_or(program);
    if bare.strip_suffix(".exe").unwrap_or(bare) != "git" {
        return false;
    }
    let mut i = 1;
    while let Some(word) = words.get(i) {
        if word == "-C" || word == "-c" {
            i += 2;
        } else if word.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    words.get(i).is_some_and(|sub| sub == "diff")
        && words[i + 1..].iter().any(|arg| {
            let lower = arg.to_ascii_lowercase();
            lower.contains("@{upstream}") || lower.contains("@{u}")
        })
}

/// `<merge-base>...HEAD` against the default branch and its diffstat.
fn correct_range(cwd: &Path) -> Option<(String, String)> {
    let git = |args: &[&str]| crate::worktrees::run_git(cwd, args).ok();
    let base = git(&[
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ])
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .or_else(|| {
        ["origin/main", "origin/master"]
            .iter()
            .find(|c| git(&["rev-parse", "--verify", "--quiet", c]).is_some())
            .map(|c| c.to_string())
    })?;
    let merge_base = git(&["merge-base", &base, "HEAD"])?.trim().to_string();
    if merge_base.is_empty() {
        return None;
    }
    let stat = git(&["diff", "--shortstat", &merge_base, "HEAD"])?
        .trim()
        .to_string();
    let short = &merge_base[..merge_base.len().min(12)];
    Some((format!("{short}...HEAD"), stat))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktrees::run_git;

    fn git(dir: &Path, args: &[&str]) -> String {
        run_git(dir, args).unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    fn commit(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x\n").unwrap();
        git(dir, &["add", name]);
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                name,
            ],
        );
    }

    /// A clone whose branch `feat` was pushed, then rebased onto a newer main:
    /// its tracking ref is stale.
    fn rebased() -> (
        kernal_api::platform::fs::TemporaryDirectory,
        std::path::PathBuf,
    ) {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let base = crate::path_norm::canonicalize_plain(tmp.path()).unwrap();
        let origin = base.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        let work = base.join("work");
        git(&base, &["clone", "-q", origin.to_str().unwrap(), "work"]);
        git(&work, &["checkout", "-q", "-b", "main"]);
        commit(&work, "base.txt");
        git(&work, &["push", "-q", "-u", "origin", "main"]);
        git(&work, &["checkout", "-q", "-b", "feat"]);
        commit(&work, "feature.txt");
        git(&work, &["push", "-q", "-u", "origin", "feat"]);
        git(&work, &["checkout", "-q", "main"]);
        for i in 0..5 {
            commit(&work, &format!("upstream-{i}.txt"));
        }
        git(&work, &["push", "-q", "origin", "main"]);
        git(&work, &["checkout", "-q", "feat"]);
        git(
            &work,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@localhost",
                "rebase",
                "-q",
                "main",
            ],
        );
        (tmp, work)
    }

    #[test]
    fn a_stale_upstream_diff_is_denied_with_the_correct_range() {
        let (_tmp, work) = rebased();
        for command in [
            "git diff @{upstream}...HEAD",
            "git diff @{u}...HEAD --stat",
            "git -C . diff --name-only @{UPSTREAM}..HEAD",
        ] {
            let why = reason(command, &work).unwrap_or_else(|| panic!("allowed `{command}`"));
            assert!(why.contains("is stale"), "{why}");
            assert!(why.contains("...HEAD"), "{why}");
            assert!(
                why.contains("1 file changed"),
                "the small real range: {why}"
            );
        }
        // The range it names is the branch's own delta from main.
        let main = git(&work, &["rev-parse", "--short=12", "main"]);
        assert!(reason("git diff @{u}...HEAD", &work)
            .unwrap()
            .contains(main.trim()));
    }

    #[test]
    fn a_fresh_upstream_and_other_commands_are_allowed() {
        let (_tmp, work) = rebased();
        // Force-push the rebased branch: the upstream is now an ancestor of HEAD.
        git(&work, &["push", "-q", "--force", "origin", "feat"]);
        assert_eq!(reason("git diff @{upstream}...HEAD", &work), None);
        // A stale ref is not the problem for commands that do not use it.
        let (_tmp2, stale) = rebased();
        for command in [
            "git diff main...HEAD",
            "git log @{upstream}..HEAD",
            "git status",
            "echo git diff @{upstream}...HEAD",
            "ls",
        ] {
            assert_eq!(reason(command, &stale), None, "{command}");
        }
    }

    #[test]
    fn no_upstream_means_nothing_to_judge() {
        let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let dir = crate::path_norm::canonicalize_plain(tmp.path()).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        commit(&dir, "a.txt");
        assert_eq!(reason("git diff @{upstream}...HEAD", &dir), None);
    }
}

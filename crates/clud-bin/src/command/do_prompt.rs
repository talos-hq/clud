//! Render the `/do` prompt for one target in one checkout (#1322).
//!
//! The `/do` skill's body is a single `` !`clud do-prompt "$ARGUMENTS"` ``
//! line. Claude Code runs it when the skill is invoked and sends its output
//! to the model in place of the skill text, so this module is the prompt. It
//! renders, from live facts:
//!
//! - the **starting point**: a verdict on the current branch (default, in
//!   progress, stale, dirty, blocked, …) and exactly what to do about it;
//! - the **target**: a meta issue routes to `/grind`, a single issue or a
//!   free-form goal gets its contract, and "can't tell" stops the session.
//!
//! `clud do` shares the classifier and [`render_current`] with `do-prompt`;
//! its `/goal` seed contains the rendered procedure, not a nested skill call.

use std::path::Path;

use serde_json::Value;

use super::do_kind::{self, DoKind};

/// What the checkout looks like right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchFacts {
    /// `None` for a detached HEAD.
    pub branch: Option<String>,
    pub default_branch: String,
    /// `git status --porcelain` lines.
    pub dirty: Vec<String>,
    /// A rebase or merge in progress, named.
    pub operation: Option<&'static str>,
    /// Commits on HEAD that are not on `origin/<default>`.
    pub ahead: u32,
    /// The newest PR whose head is this branch: `(number, state, url)`.
    pub pr: Option<(u64, String, String)>,
    /// Why the PR lookup failed, when it did.
    pub pr_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Default,
    InProgress,
    StaleMerged,
    StaleEmpty,
    Dirty,
    Unrelated,
    Blocked,
    Unknown,
}

/// Classify the starting point. Order matters: anything that could lose work
/// (a dirty tree, an operation in progress) wins over "this branch is stale".
pub fn verdict(facts: &BranchFacts, target_issue: Option<u64>) -> Verdict {
    let Some(branch) = facts.branch.as_deref() else {
        return Verdict::Blocked;
    };
    if facts.operation.is_some() {
        return Verdict::Blocked;
    }
    if !facts.dirty.is_empty() {
        return Verdict::Dirty;
    }
    if branch == facts.default_branch {
        return Verdict::Default;
    }
    if let Some((_, state, _)) = &facts.pr {
        return match state.to_ascii_uppercase().as_str() {
            "OPEN" => Verdict::InProgress,
            _ => Verdict::StaleMerged,
        };
    }
    if target_issue.is_some_and(|n| names_issue(branch, n)) {
        return Verdict::InProgress;
    }
    if facts.ahead == 0 {
        return Verdict::StaleEmpty;
    }
    if facts.pr_error.is_some() {
        return Verdict::Unknown;
    }
    Verdict::Unrelated
}

/// Whether `branch` names issue `n` as a standalone number (`fix/1234-x`,
/// `1234`, `issue-1234`), not as part of a longer one (`12345`).
pub fn names_issue(branch: &str, n: u64) -> bool {
    branch
        .split(|c: char| !c.is_ascii_digit())
        .any(|token| token.parse::<u64>().ok() == Some(n))
}

fn starting_point(facts: &BranchFacts, verdict: Verdict) -> String {
    let default = &facts.default_branch;
    let branch = facts.branch.as_deref().unwrap_or("(detached HEAD)");
    let switch = format!(
        "Before starting, run `git switch {default} && git pull --ff-only`; `{default}` is your starting branch."
    );
    match verdict {
        Verdict::Default => format!(
            "You are on `{default}`, the default branch, with a clean tree. Start here; `{default}` is your starting branch."
        ),
        Verdict::InProgress => {
            let why = match &facts.pr {
                Some((n, _, url)) => format!("its PR #{n} is open ({url})"),
                None => "its name refers to this issue".to_string(),
            };
            format!(
                "You are on `{branch}`, which is in progress: {why}. Continue on it; `{branch}` is your starting branch."
            )
        }
        Verdict::StaleMerged => {
            let (n, state, _) = facts.pr.clone().unwrap_or_default();
            format!(
                "You are on `{branch}`, but its PR #{n} is {}. That work is finished. {switch}",
                state.to_ascii_lowercase()
            )
        }
        Verdict::StaleEmpty => format!(
            "You are on `{branch}`, which has no commits beyond `origin/{default}`. {switch}"
        ),
        Verdict::Dirty => format!(
            "You are on `{branch}` with uncommitted changes:\n\n```\n{}\n```\n\nStop and ask the user what to do with them before starting. Never stash, discard or switch away from them without an answer.",
            facts.dirty.join("\n")
        ),
        Verdict::Unrelated => format!(
            "You are on `{branch}`: {} commit(s) ahead of `origin/{default}`, with no PR, and nothing ties it to this target. Ask the user whether to build on `{branch}` or to start from `{default}`, and wait for the answer.",
            facts.ahead
        ),
        Verdict::Blocked => {
            let what = match (facts.branch.as_deref(), facts.operation) {
                (None, _) => "HEAD is detached".to_string(),
                (_, Some(op)) => format!("a {op} is in progress"),
                _ => "the checkout is in an unusual state".to_string(),
            };
            format!("Stop: {what}. Tell the user and do not start work until it is resolved.")
        }
        Verdict::Unknown => format!(
            "You are on `{branch}`, and whether it has a PR could not be checked ({}). Ask the user whether `{branch}` is your starting branch or whether to start from `{default}`, and wait for the answer.",
            facts.pr_error.clone().unwrap_or_default()
        ),
    }
}

/// Shared landing guidance for normal launches and the rendered `/do` contract.
pub(super) const LAND_CONTRACT: &str = "Land the work: when a requested code change belongs in a git repository with a remote, aim to deliver a validated, pushed and merged PR. When appropriate branch from the default branch, commit the changes, and run the repository validation. Follow repository instructions and required reviews; use the merge queue when required, wait for checks with pr_merge_watch when available, fix failures, and report the merged PR URL. At PR creation, apply the repository's native-coverage labels, if any, never afterwards: a late label cancels the run. Where a merge queue is required and the harness can run background agents, you may hand the PR to a checkout-free chaperone sub-agent at push or at enqueue, then free your checkout. A chaperone handed the PR at push first waits for its required checks, then enqueues it with the head commit pinned (or merges by repository policy where there is no queue); either way it then waits for the merge and reports MERGED, NEEDS-FIXUP or STUCK; the task is complete only when the chaperone reports MERGED. On NEEDS-FIXUP, check the branch out again, fix and push, then hand it back to a chaperone, which waits for checks and enqueues again; on STUCK, report what remains. Without a required merge queue or without background agents, observe the PR yourself until it merges, per repository policy (with no hosted required checks, merge after the local gate). Reference the issue when applicable. Honor the user's explicit instructions, approval requirements, branch protection and --safe; never bypass them. If review or approval prevents merging, report what remains.";

const ISSUE_CONTRACT: &str = "\
Read the issue and implement it. Every code change keeps a RED -> GREEN \
focused regression: first show the failure or reproduction, then make that \
signal pass before the broader repository gates.

Before the first code change, inspect WORKFLOW.md and any claim-bot workflow,
including shared organization guidance, and follow the repository's claim
protocol when one exists. Resolve an unknown issue verdict manually; do not
start while blocked or needs-ready.

Re-check the live issue immediately before `gh pr create` and before enabling auto-merge or queueing.
Verify it is still open, ready when required,
and unclaimed by anyone else (including agents sharing your login). If closed,
including as not planned or a duplicate, or claimed by someone else, stop and
report why. If a GitHub read fails, stop and check manually; never assume ok.

If the issue turns out to have several independent children after all, follow \
the grind handoff above. Each child needs its own branch and one or more \
PRs; never combine children in one PR. Only a child's final PR closes it, \
and child PRs must not close the parent. Record child -> PR links, then \
close the parent after all children are resolved.

For every PR: show RED -> GREEN, review, test, push, watch CI to green, and \
merge. Merge separately, and update remaining branches as needed. Follow the \
repository's worktree rules, return to the starting branch, and leave a \
clean status.

The goal is satisfied when all PRs are merged and all referenced issues are \
closed as complete. No cheating. No files left behind. Rebase to origin main \
or master when done.";

const GOAL_CONTRACT: &str = "\
Every code change keeps a RED -> GREEN focused regression: first show the \
failure or reproduction, then make that signal pass before the broader \
repository gates.

The goal is resolved when the requested work lands in one or more PRs, each \
validated, tested, pushed and merged. Wait for the PR's GitHub Actions to go \
green, then merge it; add a watch. No cheating, no files left behind.

All work must be done for this repository. Use a git worktree or sibling \
checkout only when `/grind` and the repository's guidance allow it; work can \
only land here. When you are done, run `git status` and make sure it's \
clean, and make sure the local repo is rebased to the starting branch.

If the goal contains several independent deliverables, follow the grind \
handoff above; otherwise keep the normal `/goal` workflow.";

const GRIND_HANDOFF: &str = "\
## Grind handoff (only when needed)

Use this handoff only for a meta issue or independent children discovered \
while doing the target below. Otherwise continue with What to do.

Grind requires Claude Code's Workflow tool and the bundled grind-* agent \
types. If either is missing, stop and say: \"/grind requires the Claude \
harness; relaunch with `--harness claude`.\" Do not emulate the workflow.

Once the starting point is settled, use Read to read the complete grind router \
at the absolute path listed above (or `../grind/SKILL.md` relative to /do's \
skill base directory). Follow it with the target below as the user's argument. \
Use the router's parent directory as its base for relative leaf-skill paths. \
If it cannot be read, stop and report that failure; do not invent a procedure. \
Do not call the Skill tool: /do, /grind and the leaves remain explicit-only.\n";

/// The mandatory last step, shared with `/grind`'s router. `default` is the
/// repository's default branch.
pub fn finish_step(default: &str) -> String {
    format!(
        "## Finish (always, as the very last step)\n\n\
When the work is done, and before you report or stop, clean up the local \
repository:\n\n\
1. **No files left behind.** `git status --porcelain` prints nothing: no \
untracked files, no uncommitted changes, no stash you created (`git stash \
list`), and no temp or scratch files in the repo. Remove every worktree and \
temporary branch you created, but only after confirming its work is pushed \
or merged.\n\
2. **Rebased to the default branch.** `git fetch origin`, then \
`git switch {default}` and `git pull --ff-only origin {default}`, so the \
checkout sits on an up-to-date `origin/{default}`.\n\
3. **Report what you could not clean**, and why. Never delete work you \
did not create to get a clean status; ask instead.\n"
    )
}

/// The whole rendered prompt.
pub fn render(
    target: &str,
    kind: &Result<DoKind, String>,
    facts: &Result<BranchFacts, String>,
) -> String {
    render_with_issue(target, kind, facts, &Err("issue not inspected".into()))
}

pub(super) fn render_with_issue(
    target: &str,
    kind: &Result<DoKind, String>,
    facts: &Result<BranchFacts, String>,
    issue_status: &Result<String, String>,
) -> String {
    let target = target.trim();
    let mut out = format!(
        "# /do: rendered for this checkout by `clud do-prompt`\n\n**Target:** {}\n\n",
        if target.is_empty() {
            "(none given)"
        } else {
            target
        }
    );
    if target.is_empty() {
        out.push_str(
            "No target was given. Ask the user for an issue URL or a goal, then run `/do <target>`.\n",
        );
        return out;
    }
    let issue = do_kind::github_issue(target).map(|i| i.number);
    let default = facts
        .as_ref()
        .map(|f| f.default_branch.clone())
        .unwrap_or_else(|_| "main".to_string());

    out.push_str("## Starting point\n\n");
    match facts {
        Ok(facts) => {
            let v = verdict(facts, issue);
            out.push_str(&format!("Verdict: `{}`.\n\n", verdict_name(v)));
            out.push_str(&starting_point(facts, v));
        }
        Err(error) => out.push_str(&format!(
            "Verdict: `blocked`.\n\nStop: this checkout could not be inspected ({error}). Tell the user and do not start work."
        )),
    }
    if issue.is_some() {
        out.push_str("\n\n");
        match issue_status {
            Ok(status) => out.push_str(status),
            Err(error) => out.push_str(&format!(
                "Issue verdict: `unknown`. Stop and check manually before work: {error}."
            )),
        }
    }
    if kind.is_ok() {
        out.push_str("\n\n");
        out.push_str(GRIND_HANDOFF);
    }
    out.push_str("\n\n## What to do\n\n");
    match kind {
        Ok(DoKind::Meta) => out.push_str(
            "This is a meta issue: it has open sub-issues. Do not implement it as one change. Follow the grind handoff above.\n"
        ),
        Ok(DoKind::Single) if issue.is_some() || target.contains("://") => {
            out.push_str(ISSUE_CONTRACT);
            out.push_str("\n\n");
            out.push_str(LAND_CONTRACT);
            out.push_str("\n\n");
            out.push_str(&finish_step(&default));
        }
        Ok(DoKind::Single) => {
            out.push_str(&format!("The goal: {target}\n\n"));
            out.push_str(GOAL_CONTRACT);
            out.push_str("\n\n");
            out.push_str(LAND_CONTRACT);
            out.push_str("\n\n");
            out.push_str(&finish_step(&default));
        }
        Err(error) => out.push_str(&format!(
            "Stop. Tell the user: {error} Do not start work.\n"
        )),
    }
    out
}

pub fn verdict_name(v: Verdict) -> &'static str {
    match v {
        Verdict::Default => "default",
        Verdict::InProgress => "in_progress",
        Verdict::StaleMerged => "stale_merged",
        Verdict::StaleEmpty => "stale_empty",
        Verdict::Dirty => "dirty",
        Verdict::Unrelated => "unrelated",
        Verdict::Blocked => "blocked",
        Verdict::Unknown => "unknown",
    }
}

/// Inspect the checkout once and give both explicit entry points the same
/// procedure and installed-router context. Target text never enters a shell.
pub fn render_current(target: &str, kind: &Result<DoKind, String>) -> String {
    let facts = std::env::current_dir()
        .map_err(|error| format!("could not resolve the current directory: {error}"))
        .and_then(|cwd| gather(&cwd));
    let skills = crate::home::user_home().and_then(|home| {
        crate::skills::backend_for(crate::backend::Backend::Claude)
            .map(|backend| backend.skills_dir(&home))
    });
    let context = match skills {
        Some(skills) => format!(
            "Skill base directory for /do: {}\nGrind router path: {}\n\n",
            serde_json::json!(skills.join("do").to_string_lossy()),
            serde_json::json!(skills.join("grind").join("SKILL.md").to_string_lossy()),
        ),
        None => "Stop: the skill directory could not be resolved. Tell the user and do not start work.\n\n".to_string(),
    };
    let issue_status = super::do_issue::gather(target);
    format!(
        "{context}{}",
        render_with_issue(target, kind, &facts, &issue_status)
    )
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let mut argv = vec!["git".to_string()];
    argv.extend(args.iter().map(|a| a.to_string()));
    let (code, out) = crate::loop_spec::run_capture(argv, Some(cwd))?;
    if code == 0 {
        Ok(out.trim_end().to_string())
    } else {
        Err(format!(
            "`git {}` exited {code}: {}",
            args.join(" "),
            out.trim()
        ))
    }
}

/// Gather [`BranchFacts`] for the checkout at `cwd`.
pub fn gather(cwd: &Path) -> Result<BranchFacts, String> {
    git(cwd, &["rev-parse", "--is-inside-work-tree"])
        .map_err(|_| "this directory is not a git checkout".to_string())?;
    let head = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch = (head != "HEAD").then_some(head);
    let default_branch = git(
        cwd,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .and_then(|r| r.strip_prefix("origin/").map(str::to_string))
    .or_else(|| {
        ["main", "master"].into_iter().find_map(|name| {
            git(
                cwd,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/remotes/origin/{name}"),
                ],
            )
            .ok()
            .map(|_| name.to_string())
        })
    })
    .unwrap_or_else(|| "main".to_string());
    let dirty = git(cwd, &["status", "--porcelain"])?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    let operation = [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("MERGE_HEAD", "merge"),
    ]
    .into_iter()
    .find(|(path, _)| {
        git(cwd, &["rev-parse", "--git-path", path])
            .map(|p| cwd.join(p).exists())
            .unwrap_or(false)
    })
    .map(|(_, name)| name);
    let ahead = git(
        cwd,
        &[
            "rev-list",
            "--count",
            &format!("origin/{default_branch}..HEAD"),
        ],
    )
    .ok()
    .and_then(|n| n.trim().parse().ok())
    .unwrap_or(0);
    let mut facts = BranchFacts {
        branch,
        default_branch,
        dirty,
        operation,
        ahead,
        pr: None,
        pr_error: None,
    };
    if let Some(branch) = facts.branch.clone().filter(|b| *b != facts.default_branch) {
        match pr_for_branch(&branch) {
            Ok(pr) => facts.pr = pr,
            Err(error) => facts.pr_error = Some(error),
        }
    }
    Ok(facts)
}

fn pr_for_branch(branch: &str) -> Result<Option<(u64, String, String)>, String> {
    let (code, out) = crate::loop_spec::run_gh_capture(&[
        "pr",
        "list",
        "--head",
        branch,
        "--state",
        "all",
        "--json",
        "number,state,url",
        "--limit",
        "5",
    ])?;
    if code != 0 {
        return Err(format!("`gh pr list` exited {code}: {}", out.trim()));
    }
    let prs: Value =
        serde_json::from_str(&out).map_err(|e| format!("unparseable gh output: {e}"))?;
    let newest = prs
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some((
                p.get("number")?.as_u64()?,
                p.get("state")?.as_str()?.to_string(),
                p.get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ))
        })
        .max_by_key(|(n, _, _)| *n);
    Ok(newest)
}

/// `clud do-prompt <target…>`: print the rendered prompt; always exit 0 so
/// the harness shows the model the text (a refusal is part of the text).
pub fn run(words: &[String]) -> i32 {
    let target = words.join(" ");
    let kind = if target.trim().is_empty() {
        Ok(DoKind::Single)
    } else {
        do_kind::classify(
            &target,
            std::env::var(do_kind::DO_KIND_ENV).ok().as_deref(),
            do_kind::query_sub_issues,
        )
    };
    print!("{}", render_current(&target, &kind));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(branch: &str) -> BranchFacts {
        BranchFacts {
            branch: Some(branch.to_string()),
            default_branch: "main".into(),
            ..BranchFacts::default()
        }
    }

    #[test]
    fn land_contract_scopes_the_chaperone_handoff_to_required_queues() {
        assert!(LAND_CONTRACT.contains("only when the chaperone reports MERGED"));
        assert!(LAND_CONTRACT.contains("Without a required merge queue"));
        assert!(LAND_CONTRACT.contains("merge after the local gate"));
        assert!(LAND_CONTRACT.contains("NEEDS-FIXUP, check the branch out again"));
        assert!(LAND_CONTRACT.contains("on STUCK, report what remains"));
        assert!(LAND_CONTRACT.contains("without background agents, observe the PR yourself"));
        assert!(LAND_CONTRACT.contains("first waits for its required checks, then enqueues it"));
        assert!(!LAND_CONTRACT.contains("ci-windows"));
    }

    #[test]
    fn verdict_table() {
        assert_eq!(verdict(&facts("main"), None), Verdict::Default);

        let mut open = facts("feat/x");
        open.pr = Some((12, "OPEN".into(), "u".into()));
        open.ahead = 3;
        assert_eq!(verdict(&open, None), Verdict::InProgress);

        let mut merged = facts("feat/x");
        merged.pr = Some((12, "MERGED".into(), "u".into()));
        assert_eq!(verdict(&merged, None), Verdict::StaleMerged);

        assert_eq!(verdict(&facts("feat/x"), None), Verdict::StaleEmpty);

        let mut ahead = facts("feat/x");
        ahead.ahead = 2;
        assert_eq!(verdict(&ahead, None), Verdict::Unrelated);
        assert_eq!(verdict(&ahead, Some(7)), Verdict::Unrelated);
        let mut named = facts("fix/7-thing");
        named.ahead = 2;
        assert_eq!(verdict(&named, Some(7)), Verdict::InProgress);

        let mut unknown = ahead.clone();
        unknown.pr_error = Some("gh: not signed in".into());
        assert_eq!(verdict(&unknown, None), Verdict::Unknown);
    }

    #[test]
    fn risky_states_win_over_staleness() {
        let mut dirty = facts("feat/x");
        dirty.pr = Some((1, "MERGED".into(), "u".into()));
        dirty.dirty = vec![" M src/lib.rs".into()];
        assert_eq!(verdict(&dirty, None), Verdict::Dirty);

        let mut rebasing = dirty.clone();
        rebasing.operation = Some("rebase");
        assert_eq!(verdict(&rebasing, None), Verdict::Blocked);

        let detached = BranchFacts {
            branch: None,
            ..facts("main")
        };
        assert_eq!(verdict(&detached, None), Verdict::Blocked);
    }

    #[test]
    fn issue_numbers_in_branch_names_match_whole_numbers_only() {
        assert!(names_issue("fix/1234-slow", 1234));
        assert!(names_issue("issue-1234", 1234));
        assert!(!names_issue("fix/12345", 1234));
        assert!(!names_issue("feat/x", 1234));
    }

    #[test]
    fn render_single_issue_on_default() {
        let out = render(
            "https://github.com/o/r/issues/7",
            &Ok(DoKind::Single),
            &Ok(facts("main")),
        );
        assert!(out.contains("Verdict: `default`"));
        assert!(out.contains("Read the issue and implement it"));
        assert!(out.contains("never combine children in one PR"));
        assert!(!out.contains("/grind https://"));
    }

    #[test]
    fn render_stale_merged_says_switch_and_names_the_pr() {
        let mut merged = facts("feat/x");
        merged.pr = Some((812, "MERGED".into(), "u".into()));
        let out = render(
            "https://github.com/o/r/issues/7",
            &Ok(DoKind::Single),
            &Ok(merged),
        );
        assert!(out.contains("Verdict: `stale_merged`"));
        assert!(out.contains("PR #812 is merged"));
        assert!(out.contains("git switch main && git pull --ff-only"));
    }

    #[test]
    fn contracts_end_with_the_finish_step_on_the_real_default_branch() {
        let mut on_master = facts("master");
        on_master.default_branch = "master".into();
        for target in ["https://github.com/o/r/issues/7", "refactor the launcher"] {
            let out = render(target, &Ok(DoKind::Single), &Ok(on_master.clone()));
            let finish = out
                .find("## Finish (always, as the very last step)")
                .expect(target);
            assert!(
                finish > out.find("## What to do").unwrap(),
                "finish comes last"
            );
            assert!(out.contains("`git status --porcelain` prints nothing"));
            assert!(out.contains("`git pull --ff-only origin master`"));
        }
        // A stop has no finish step: nothing was started.
        let stop = render(
            "https://github.com/o/r/issues/7",
            &Err("gh down.".into()),
            &Ok(on_master),
        );
        assert!(!stop.contains("## Finish"));
    }

    #[test]
    fn render_meta_routes_to_grind() {
        let out = render(
            "https://github.com/o/r/issues/9",
            &Ok(DoKind::Meta),
            &Ok(facts("main")),
        );
        assert!(out.contains("Follow the grind handoff above"));
        assert!(out.contains("use Read to read the complete grind router"));
        assert!(out.contains("Workflow tool"));
        assert!(out.contains("Do not call the Skill tool"));
        assert!(!out.contains("Read the issue and implement it"));
    }

    #[test]
    fn render_cant_tell_stops() {
        let out = render(
            "https://github.com/o/r/issues/9",
            &Err("gh is not signed in.".into()),
            &Ok(facts("main")),
        );
        assert!(out.contains("Stop. Tell the user: gh is not signed in."));
        assert!(!out.contains("Read the issue and implement it"));
    }

    #[test]
    fn render_free_form_goal() {
        let out = render(
            "refactor the launcher",
            &Ok(DoKind::Single),
            &Ok(facts("main")),
        );
        assert!(out.contains("The goal: refactor the launcher"));
        assert!(out.contains("validated, tested, pushed and merged"));
    }

    #[test]
    fn render_dirty_lists_changes_and_asks() {
        let mut dirty = facts("main");
        dirty.dirty = vec![" M README.md".into()];
        let out = render("x", &Ok(DoKind::Single), &Ok(dirty));
        assert!(out.contains("Verdict: `dirty`"));
        assert!(out.contains(" M README.md"));
        assert!(out.contains("Stop and ask the user"));
    }
}

#[cfg(test)]
mod issue_preflight_regression {
    #[test]
    fn issue_read_failure_never_renders_ok() {
        let out = super::render(
            "https://github.com/o/r/issues/7",
            &Ok(super::DoKind::Single),
            &Ok(super::BranchFacts::default()),
        );
        assert!(out.contains("Issue verdict: `unknown`"), "{out}");
        assert!(out.contains("check manually"), "{out}");
    }
}

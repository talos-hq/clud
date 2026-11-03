use crate::backend::Backend;

pub(super) const FIX_PROMPT: &str = "\
Look for linting like ./lint, or npm or python, choose the most likely one, \
then look for unit tests like ./test or pytest or npm test, run the most likely one. \
For each stage fix until it works, rerunning it until it does.";

pub(super) const GITHUB_FIX_VALIDATION: &str = "\
run `lint-test` upto 5 times, fixing on each time or until it passes. \
If you run into a locked file then try two times, same with misc system error. Else halt.";

pub(super) const GITHUB_FIX_TEMPLATE: &str = "\
First, download the logs from the GitHub URL: {url}

IMPORTANT: Use the global `gh` tool to download the logs. For example:
- For workflow runs: `gh run view <run_id> --log`
- For pull requests: `gh pr checks <pr_number> --watch` or `gh pr view <pr_number>`

If the `gh` tool is not found, warn the user that the GitHub CLI tool is not available and fall back to \
using other methods such as curl or web requests to fetch the relevant information from the GitHub API or \
page content.

After downloading and analyzing the logs:
1. Generate a recommended fix based on the errors/issues found
2. List all the steps required to implement the fix
3. Execute the fix by implementing each step

Then proceed with the validation process:
{validation}";

const DO_NOT_PREPARED: &str =
    "Stop: the live /do procedure was not prepared. Tell the user and do not start work.";

/// Claude Code rejects a `/goal` condition over this many characters with a
/// request-free reply, so the session silently does nothing (#2372).
pub(super) const CLAUDE_GOAL_CONDITION_LIMIT: usize = 4000;

/// Opening of Claude's `/do` goal condition; the target follows it.
const DO_GOAL_LEAD: &str = "Complete `clud do` for the target below by following the /do \
procedure rendered by `clud do-prompt` in your system prompt, through its Finish step.";

/// The text before the target in Claude's `/do` goal. `brief_path` names the
/// recorded full target when the goal is a compaction of it (#2522).
pub(crate) fn do_goal_preamble(brief_path: Option<&str>) -> String {
    match brief_path {
        Some(path) => format!(
            "{DO_GOAL_LEAD} The full target is saved at {path} and quoted in your system \
prompt; execute it exactly. Target: "
        ),
        None => format!("{DO_GOAL_LEAD} Target: "),
    }
}

/// The rendered `clud do-prompt` contract, or a fail-closed stop instruction.
pub(super) fn do_procedure(rendered: Option<&str>) -> &str {
    rendered
        .filter(|text| !text.trim().is_empty())
        .unwrap_or(DO_NOT_PREPARED)
}

/// Keep the rendered contract as goal text. Neither nested slash-command
/// expansion nor a model-side Skill invocation is supported here (#2013).
/// Used for Codex; Claude caps the condition, see [`build_claude_do_goal`].
pub(super) fn build_do_prompt(rendered: Option<&str>) -> String {
    format!("/goal {}", do_procedure(rendered))
}

/// Claude's `/goal` seed: a condition naming the target. The rendered
/// procedure travels in the appended system prompt, so the first request
/// still carries it while the condition stays under Claude Code's cap (#2372).
///
/// A target too long to quote keeps its hard constraints verbatim (#2522);
/// `main` normally prepares a model-compacted `args.do_goal` instead, and this
/// deterministic path covers plans built without that step.
pub(super) fn build_claude_do_goal(target: &str, prepared: bool) -> String {
    if !prepared {
        return format!("/goal {DO_NOT_PREPARED}");
    }
    match super::goal_compact::compact_goal(&do_goal_preamble(None), target, |_| {
        Err("no model at plan time".into())
    }) {
        Ok(goal) => format!("/goal {}", goal.condition),
        Err(error) => format!("/goal Stop: {error}. Tell the user and do not start work."),
    }
}

pub(super) const REBASE_PROMPT: &str = "\
First, unconditionally run `git fetch` to update all remote branches. \
Then rebase to the current origin head. Use the git tool to figure out \
what the origin is. If there is no rebase then do a pull and attempt \
to do a rebase, if it's not successful then finish the rebase line \
by line, don't revert any files. After that print out a summary of \
what you did to make it work, or just say \"No rebase necessary\".";

pub(super) const UP_PROMPT: &str = "\
You are preparing this repo for a commit to master. Follow these steps:

1. Run `bash lint` (or equivalent linting command for this repo). \
If it fails, fix all errors and rerun until it passes.

2. Run `bash test` (or equivalent test command for this repo). \
If it fails, fix all errors and rerun until it passes.

3. Remove all slop and temporary files: leftover debug prints, \
TODO/FIXME comments you introduced, .bak files, __pycache__ dirs, \
temp files, and any other artifacts that shouldn't be committed.

4. After lint and tests pass, review the git diff and come up with \
a concise one-line summary describing what changed in this repo.

5. Every 30 seconds while working, output a brief status summary of \
what you're doing and current pass/fail state.

6. Once everything passes and is clean, run:
   codeup -m \"<your one-line summary>\"
   (codeup is a global command installed on the system)

7. If codeup fails, read the output, investigate and fix the breakage, \
then rerun lint and test again to make sure fixes didn't break anything, \
and retry codeup. Repeat up to 5 times before giving up.

8. If codeup succeeds (exit code 0), halt.";

pub(super) const UP_CODEUP_STEP_MARKER: &str =
    "6. Once everything passes and is clean, run:\n   codeup -m \"<your one-line summary>\"";

/// Push a prompt into `cmd` using the right convention for the backend.
/// Claude uses `-p <prompt>`; codex takes the prompt as a positional argument
/// (either to `codex exec` or to the interactive TUI).
pub(super) fn push_prompt(cmd: &mut Vec<String>, backend: Backend, prompt: String) {
    match backend {
        Backend::Claude => {
            cmd.push("-p".to_string());
            cmd.push(prompt);
        }
        Backend::Codex => {
            cmd.push(prompt);
        }
        Backend::DeepSeek => {
            cmd.push(prompt);
        }
    }
}

/// Seed an *interactive* session with `prompt` instead of running it headless.
///
/// `push_prompt` uses claude's `-p/--print` mode, which runs the prompt as a
/// single non-interactive shot and buffers everything until it finishes. That
/// is wrong for `/goal`-style contracts that drive a long Stop-hook loop: the
/// terminal shows nothing for minutes and looks halted (the `clud do` "nothing
/// happened" report). Claude treats a bare positional `[prompt]` as the opening
/// message of an interactive session, so we simply omit `-p`. Codex already
/// takes the prompt positionally, so it is unchanged.
pub(super) fn push_prompt_interactive(cmd: &mut Vec<String>, prompt: String) {
    cmd.push(prompt);
}

pub(super) fn build_up_prompt(message: Option<&str>, publish: bool) -> String {
    let mut prompt = UP_PROMPT.to_string();

    match (message, publish) {
        (Some(msg), true) => {
            let replacement = format!(
                "6. Once everything passes and is clean, run:\n   codeup -m \"{}\" -p",
                msg
            );
            prompt = prompt.replace(UP_CODEUP_STEP_MARKER, &replacement);
        }
        (Some(msg), false) => {
            let replacement = format!(
                "6. Once everything passes and is clean, run:\n   codeup -m \"{}\"",
                msg
            );
            prompt = prompt.replace(UP_CODEUP_STEP_MARKER, &replacement);
        }
        (None, true) => {
            let replacement =
                "6. Once everything passes and is clean, run:\n   codeup -m \"<your one-line summary>\" -p";
            prompt = prompt.replace(UP_CODEUP_STEP_MARKER, replacement);
        }
        (None, false) => {}
    }

    prompt
}

/// `clud grind` hands the whole run to the `/grind` router skill, which asks
/// for the mode (parallel, sequential, or cron via the harness's own `/loop`)
/// and drives the bundled `grind-run` workflow. See `docs/architecture/grind.md`.
pub(super) const GRIND_TEMPLATE: &str = "/grind {url}";

pub(super) fn build_grind_prompt(url: &str) -> String {
    GRIND_TEMPLATE.replace("{url}", url)
}

pub(super) fn build_fix_prompt(url: Option<&str>) -> String {
    match url {
        Some(u) if is_github_url(u) => GITHUB_FIX_TEMPLATE
            .replace("{url}", u)
            .replace("{validation}", GITHUB_FIX_VALIDATION),
        _ => FIX_PROMPT.to_string(),
    }
}

pub(super) fn is_github_url(url: &str) -> bool {
    url.starts_with("https://github.com/") || url.starts_with("http://github.com/")
}

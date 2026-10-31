//! zackees/clud#1979: one GitHub or remote mutation per command, and never a
//! mutation together with a long-running watch.
//!
//! A chained `git push ...; gh pr create ...; <watch>` buries a failed
//! mutation under minutes of watch output (or drops its exit status in a
//! `| tail`), hides the PR URL the watch needs, and re-runs the mutation when
//! the watch is retried. Arguments, heredoc bodies and `--body` text are not
//! commands, so prose that names `gh pr merge` never trips the rule.

use super::block_bad_cmd_wait_scan::peel_wrappers;
use super::*;
use crate::shell_ast::{Invocation, Script};

/// Rule id for `CLUD_BAD_CMD_OVERRIDE=mutation-chain:<reason>`.
pub(super) const MUTATION_CHAIN_RULE_ID: &str = "mutation-chain";

const WATCH_SCRIPTS: &[&str] = &["pr_merge_watch.py", "pr_merge_watch", "run_watch.py"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Push,
    PrCreate,
    Mutation,
    /// `gh api` with a non-GET method: one REST call per invocation.
    ApiWrite,
    /// Issue metadata write: a small independent REST call with no PR URL or
    /// CI to bury.
    IssueWrite,
    Watch,
}

fn classify(words: &[String]) -> Option<Kind> {
    if super::native_gh_waiter(words) {
        return Some(Kind::Watch);
    }
    if words.iter().any(|word| {
        WATCH_SCRIPTS
            .iter()
            .any(|script| word.ends_with(script) || word.ends_with(&format!("{script}\"")))
    }) {
        return Some(Kind::Watch);
    }
    let program = program_name(words.first()?);
    if program == "git" {
        let positionals: Vec<&str> = words[1..]
            .iter()
            .map(String::as_str)
            .filter(|word| !word.starts_with('-'))
            .collect();
        return (positionals.first() == Some(&"push")).then_some(Kind::Push);
    }
    if program != "gh" {
        return None;
    }
    let positionals = super::gh_positionals(&words[1..]);
    match positionals.as_slice() {
        ["pr", "create", ..] => Some(Kind::PrCreate),
        ["pr", "edit" | "merge" | "close" | "ready" | "review", ..]
        | ["release", "create", ..]
        | ["workflow", "run", ..] => Some(Kind::Mutation),
        ["issue", "create" | "edit" | "close" | "reopen" | "comment", ..] => Some(Kind::IssueWrite),
        ["api", ..] if gh_api_writes(words) => Some(Kind::ApiWrite),
        _ => None,
    }
}

/// `gh api` with an explicit non-GET method.
fn gh_api_writes(words: &[String]) -> bool {
    words.iter().enumerate().any(|(index, word)| {
        let method = match word.as_str() {
            "-X" | "--method" => words.get(index + 1).map(String::as_str),
            other => other
                .strip_prefix("--method=")
                .or_else(|| other.strip_prefix("-X").filter(|rest| !rest.is_empty())),
        };
        method.is_some_and(|method| !method.eq_ignore_ascii_case("GET"))
    })
}

/// `git push ... && gh pr create ...`, unpiped and in that order: `&&` stops
/// on a failed push and the create prints the PR URL, so the pair is allowed.
fn push_then_create(found: &[(&Invocation, Kind)], command_text: &str) -> bool {
    let [(push, Kind::Push), (create, Kind::PrCreate)] = found else {
        return false;
    };
    if push.pipeline.is_some() || create.pipeline.is_some() || push.loop_kind.is_some() {
        return false;
    }
    let Some(start) = command_text.find(push.source.as_str()) else {
        return false;
    };
    let after = start + push.source.len();
    command_text[after..]
        .find(create.source.as_str())
        .is_some_and(|gap| command_text[after..after + gap].trim() == "&&")
}

/// #2354: a batch made only of `gh api` writes and `gh issue` metadata writes
/// (including loops over them). Each write is an independent small REST call
/// with no PR URL or CI to bury, so splitting the batch only costs round trips.
fn metadata_batch(found: &[(&Invocation, Kind)]) -> bool {
    !found.is_empty()
        && found
            .iter()
            .all(|(_, kind)| matches!(kind, Kind::ApiWrite | Kind::IssueWrite))
}

pub(super) fn mutation_chain_reason(
    script: &Script,
    command_text: &str,
    evaluation: &mut CommandEvaluation,
) -> Option<String> {
    let found: Vec<(&Invocation, Kind)> = script
        .invocations
        .iter()
        .filter(|invocation| invocation.origin != crate::shell_ast::Origin::FunctionBody)
        .filter_map(|invocation| {
            classify(&peel_wrappers(&rule_words(invocation)).words).map(|kind| (invocation, kind))
        })
        .collect();
    let mutations = if metadata_batch(&found) {
        1
    } else {
        found
            .iter()
            .map(|(invocation, kind)| match kind {
                Kind::Watch => 0,
                // A mutation in a loop body runs once per iteration.
                _ if invocation.loop_kind.is_some() => 2,
                _ => 1,
            })
            .sum::<usize>()
    };
    let watch = found.iter().any(|(_, kind)| *kind == Kind::Watch);
    let refused =
        (mutations >= 1 && watch) || (mutations >= 2 && !push_then_create(&found, command_text));
    if !refused {
        return None;
    }
    if let Some(override_reason) = accepted_override_reason(MUTATION_CHAIN_RULE_ID) {
        evaluation.log_messages.push(format!(
            "BAD_CMD_OVERRIDE accepted rule={MUTATION_CHAIN_RULE_ID} reason={override_reason:?} command={command_text:?}"
        ));
        return None;
    }
    Some(format!(
        "[{MUTATION_CHAIN_RULE_ID}] Do not chain GitHub mutations with each other or with a CI watch in one command (zackees/clud#1979): a failed push or PR create gets buried under the watch output, the PR URL is lost, and retrying the watch repeats the mutation. Run the mutation (`git push ...` or `gh pr create ...`) alone, without piping it through tail/grep, and read its exit status and PR URL. Then run `clud tool run github/pr_merge_watch.py <PR>` as a separate command, preferably with run_in_background. Two shapes are allowed: `git push ... && gh pr create ...` (unpiped, in that order), and a batch made only of `gh api` writes and `gh issue create|edit|close|reopen|comment` metadata writes, loops over them included (#2423); neither may be combined with a watch. To intentionally bypass this rule for this one command, set the real environment variable {BAD_CMD_OVERRIDE_ENV}=\"{MUTATION_CHAIN_RULE_ID}:<your reason>\" for this tool call and re-run the exact same command unchanged."
    ))
}

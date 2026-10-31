//! Per-role shell caps for the `/grind` workflow's agents.
//!
//! Claude Code puts `agent_type` in a subagent's PreToolUse payload. When it
//! names one of the bundled `grind-*` roles, the role's shell allowlist (or,
//! for the integrator, denylist) decides the call. Every other caller —
//! the primary session, other subagents — is untouched, except that while
//! the run facts record a feature branch the router caps ([`router_reason`])
//! apply to it: the main-session router carries no agent type to key on.
//!
//! The agent definitions' `tools:` frontmatter is the hard limit on *which
//! tools* a role has (workers and reviewers get a shell only for read-only
//! `gh`); this module
//! is the second layer for the one tool frontmatter cannot narrow. See
//! [`docs/architecture/grind.md`](../../../docs/architecture/grind.md).
//!
//! Run-scoped facts come from the run-facts file the `/grind` router writes
//! before the workflow starts, `~/.clud/tmp/grind/<session_id>.json`
//! ([`crate::grind_facts`], #1337). The hook reads the file for the payload's
//! `session_id`, which a workflow agent shares with its parent session, so
//! concurrent runs never see each other's facts. The facts are `mode`
//! (`parallel` permits the planner's `git worktree add`) and `ci` (permits the
//! integrator's `act`), plus the optional `scripts: {lint, test}` the router
//! detected. Workers and reviewers may never run the repo's lint/test
//! scripts; the integrator runs them before every push. During the read-only
//! planning pass the router also writes `"phase": "plan"`, which puts the
//! planner in plan-only mode (no worktrees, no file writes); the router
//! removes it before the real run. With no facts for the session, the
//! defaults (sequential, no CI) are the strictest caps.

use std::path::Path;

use serde_json::Value;

use super::block_bad_cmd_gate::statement_words;

pub(super) const PLANNER: &str = "grind-planner";
pub(super) const WORKER: &str = "grind-worker";
pub(super) const REVIEWER: &str = "grind-reviewer";
pub(super) const INTEGRATOR: &str = "grind-integrator";
pub(super) const LANDER: &str = "grind-lander";
pub(super) const PREWORK: &str = "grind-prework";
pub(super) const ORCHESTRATOR: &str = "grind-orchestrator";
pub(super) const GATE: &str = "grind-gate";
pub(super) const CHAPERONE: &str = "grind-chaperone";

/// Read-only inspection commands every shell-holding role may run, subject
/// to [`inspect_ok`].
const INSPECT: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "find", "grep", "rg", "pwd", "echo", "test", "true",
];

const GIT_READ: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "rev-parse",
    "ls-files",
    "fetch",
    "merge-base",
    "describe",
    "cat-file",
    "rev-list",
    "blame",
    "grep",
];

/// `find -exec`/`-delete` run or remove things, and `tail -f` never exits.
/// A path-qualified program (`./test`, `bin/ls`) is never the builtin or the
/// system tool, so it is not inspection.
fn inspect_ok(program: &str, words: &[String]) -> bool {
    if !INSPECT.contains(&program) || words[0].contains(['/', '\\']) {
        return false;
    }
    let args = &words[1..];
    match program {
        "find" => !args.iter().any(|w| {
            matches!(
                w.as_str(),
                "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete"
            ) || w.starts_with("-fprint")
                || w.starts_with("-fls")
        }),
        "tail" => !args.iter().any(|w| {
            w == "--follow"
                || w.starts_with("--follow=")
                || (w.starts_with('-')
                    && !w.starts_with("--")
                    && (w.contains('f') || w.contains('F')))
        }),
        _ => true,
    }
}

/// The run facts the router recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RunFacts {
    pub parallel: bool,
    pub ci: bool,
    /// `"phase": "plan"`: the router's read-only planning pass.
    pub plan_only: bool,
    /// The repo's lint/test scripts the router detected and the user agreed
    /// to run (`scripts: {lint, test}`); `None` when absent or not a string.
    pub scripts: RunScripts,
    /// The meta issue number (`meta`, a JSON number or string, `#` stripped):
    /// the one issue `grind-prework` may comment on.
    pub meta: Option<String>,
    /// In a multi-repository run (#1422), the `meta` of every `repos` entry:
    /// each sister lane's tracker, which its `grind-prework` comments on even
    /// when the hook's `cwd` resolves to another entry or none (#2468).
    pub lane_metas: Vec<String>,
    /// Feature-branch mode (`feature: {branch, pr, worktree}`); `None` when
    /// absent, which keeps every cap exactly as in a plain run.
    pub feature: Option<FeatureFacts>,
    /// What happens to the feature PR once every goal PR has landed.
    pub feature_merge: FeatureMerge,
    /// Worker tasks the planner recorded (`clud grind-facts task`): the
    /// deletion roots of a worker or reviewer in that checkout (#1340).
    pub tasks: Vec<TaskFacts>,
}

/// One recorded worker task: its checkout and the files it owns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TaskFacts {
    pub worktree: std::path::PathBuf,
    pub files: Vec<String>,
}

/// The feature branch a feature-branch-mode run lands its goal PRs into.
///
/// The main-session router keeps a single feature worktree (`worktree`);
/// [`router_reason`] refuses any other `git worktree add` from a caller that
/// is not a `grind-*` role while the feature is recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct FeatureFacts {
    pub branch: String,
    /// The draft feature PR number (`#` or URL stripped). While the run facts
    /// records the feature, its PR is treated as open.
    pub pr: Option<String>,
    pub worktree: Option<String>,
}

/// `feature_merge` in the run facts: whether the lander may merge the feature PR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum FeatureMerge {
    /// The lander readies and merges the feature PR (merge commit only).
    Auto,
    /// Leave the PR for the user to decide.
    #[default]
    DecideLater,
    /// Comment on the PR only; the user merges it.
    CommentOnly,
}

/// The `meta` issue number of one facts object (a JSON number or string,
/// `#` stripped).
fn meta_number(value: &Value) -> Option<String> {
    value.get("meta").and_then(|m| match m {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => {
            let s = s.trim();
            let s = s.strip_prefix('#').unwrap_or(s).trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        _ => None,
    })
}

/// Every `repos` entry's meta in a multi-repository facts file (#2468);
/// empty for a single-repository file.
pub(super) fn lane_metas(facts: &Value) -> Vec<String> {
    facts
        .get("repos")
        .and_then(Value::as_array)
        .map(|repos| repos.iter().filter_map(meta_number).collect())
        .unwrap_or_default()
}

/// A PR number from a JSON number, `#n`, `n`, or a URL ending in the number.
fn pr_number(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => number_word(s),
        _ => None,
    }
}

fn number_word(word: &str) -> Option<String> {
    let bare = word
        .trim()
        .trim_matches(&['\'', '"'][..])
        .trim_end_matches('/');
    let tail = bare.rsplit('/').next().unwrap_or(bare);
    let num = tail.strip_prefix('#').unwrap_or(tail).trim();
    (!num.is_empty() && num.chars().all(|c| c.is_ascii_digit())).then(|| num.to_string())
}

/// The repo scripts recorded in the run facts' optional `scripts` object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RunScripts {
    pub lint: Option<String>,
    pub test: Option<String>,
}

impl RunFacts {
    pub(super) fn from_json(value: &Value) -> Self {
        let script = |name: &str| {
            value
                .get("scripts")
                .and_then(|s| s.get(name))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Self {
            parallel: value.get("mode").and_then(Value::as_str) == Some("parallel"),
            ci: value.get("ci").and_then(Value::as_bool).unwrap_or(false),
            plan_only: value.get("phase").and_then(Value::as_str) == Some("plan"),
            scripts: RunScripts {
                lint: script("lint"),
                test: script("test"),
            },
            meta: meta_number(value),
            lane_metas: Vec::new(),
            feature: value.get("feature").and_then(|f| {
                let branch = f.get("branch")?.as_str()?.trim();
                (!branch.is_empty()).then(|| FeatureFacts {
                    branch: branch.to_string(),
                    pr: f.get("pr").and_then(pr_number),
                    worktree: f
                        .get("worktree")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                })
            }),
            feature_merge: match value.get("feature_merge").and_then(Value::as_str) {
                Some("auto") => FeatureMerge::Auto,
                Some("comment_only" | "comment") => FeatureMerge::CommentOnly,
                _ => FeatureMerge::DecideLater,
            },
            tasks: value
                .get("tasks")
                .and_then(Value::as_array)
                .map(|tasks| {
                    tasks
                        .iter()
                        .filter_map(|task| {
                            let worktree = task.get("worktree")?.as_str()?;
                            let files = task
                                .get("files")?
                                .as_array()?
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect();
                            Some(TaskFacts {
                                worktree: worktree.into(),
                                files,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// The directories of the files recorded for tasks in `checkout`, or
    /// `None` when the planner recorded none there.
    fn task_dirs(&self, checkout: &Path) -> Option<Vec<std::path::PathBuf>> {
        let dirs: Vec<std::path::PathBuf> = self
            .tasks
            .iter()
            .filter(|task| normalize(&task.worktree) == checkout)
            .flat_map(|task| {
                task.files.iter().filter_map(|file| {
                    normalize(&task.worktree.join(file))
                        .parent()
                        .map(Path::to_path_buf)
                })
            })
            .collect();
        (!dirs.is_empty()).then_some(dirs)
    }

    /// The feature PR number, when a feature run records one.
    fn feature_pr(&self) -> Option<&str> {
        self.feature.as_ref()?.pr.as_deref()
    }

    /// The facts of the run that `session_id` belongs to (#1337), read from
    /// `~/.clud/tmp/grind/<session_id>.json` and narrowed to the repository
    /// `cwd` is in (#1422). With none (no id, no file, a stale or unreadable
    /// one, or no single matching repository) the defaults apply, which are
    /// the strictest caps, and the second value says why for the hook log.
    pub(super) fn for_session(session_id: Option<&str>, cwd: &Path) -> (Self, Option<String>) {
        let whole = crate::grind_facts::lookup(session_id);
        let lane_metas = whole.facts().map(lane_metas).unwrap_or_default();
        let (mut run, warning) = Self::from_lookup(&crate::grind_facts::narrow(whole, cwd));
        run.lane_metas = lane_metas;
        (run, warning)
    }

    /// Whether `target` is an issue `grind-prework` may comment on: the
    /// run's meta, or in a multi-repository run any lane's meta (#2468).
    fn is_comment_host(&self, target: &str) -> bool {
        self.meta.as_deref() == Some(target) || self.lane_metas.iter().any(|m| m == target)
    }

    pub(super) fn from_lookup(lookup: &crate::grind_facts::Lookup) -> (Self, Option<String>) {
        (
            lookup.facts().map(Self::from_json).unwrap_or_default(),
            lookup.warning(),
        )
    }
}

/// Set to `1` to let an `Agent` call start a `grind-*` agent type directly,
/// for testing one role by hand.
pub(super) const ALLOW_GRIND_AGENTS_ENV: &str = "CLUD_ALLOW_GRIND_AGENTS";

/// The denial for an `Agent` call that names a `grind-*` agent type, or
/// `None` to allow. The workflow never uses the `Agent` tool (its `agent()`
/// calls emit no tool event), so this only ever stops the model delegating
/// to a grind role on its own.
pub(super) fn agent_spawn_reason(
    tool_name: &str,
    tool_input: Option<&Value>,
    allow_env: Option<&str>,
) -> Option<String> {
    if !super::creates_subagent(tool_name) || allow_env == Some("1") {
        return None;
    }
    // Codex's `spawn_agent` names the role `agent_type` (#1954).
    let requested = ["subagent_type", "subagentType", "agent_type"]
        .iter()
        .find_map(|key| tool_input?.get(*key))?
        .as_str()?;
    (is_grind_role(requested) && requested != ORCHESTRATOR).then(|| {
        format!(
            "Blocked: `{requested}` is an internal /grind role, started only by the \
             `grind-run` workflow. Use /grind to run the workflow, or do the work \
             yourself. To start this role by hand for testing, set \
             {ALLOW_GRIND_AGENTS_ENV}=1."
        )
    })
}

/// Whether `agent_type` is a capped `/grind` role.
pub(super) fn is_grind_role(agent_type: &str) -> bool {
    matches!(
        agent_type,
        PLANNER
            | WORKER
            | REVIEWER
            | INTEGRATOR
            | LANDER
            | PREWORK
            | ORCHESTRATOR
            | GATE
            | CHAPERONE
    )
}

/// The denial reason for a shell `command` from `role`, or `None` to allow.
pub(super) fn shell_reason(role: &str, command: &str, run: &RunFacts) -> Option<String> {
    if role == GATE {
        return Some(format!(
            "{GATE} requires one approved check bound to its native agent identity"
        ));
    }
    if role == ORCHESTRATOR {
        return orchestrator_shell_reason(command);
    }
    let statements = match statement_words(command) {
        Ok(statements) => statements,
        Err(construct) => return Some(format!("{role} may not use {construct}")),
    };
    if role == CHAPERONE {
        return statements
            .iter()
            .map(|words| strip_gh_repo_flag(words))
            .find_map(|words| chaperone_reason(&words));
    }
    if role == INTEGRATOR && is_script_retry_loop(&statements) {
        return Some(format!(
            "{role} may not rerun lint or test in a shell loop: read the failing tests first, and \
             rerun unchanged only an infrastructure error, at most twice (#1425)"
        ));
    }
    statements
        .iter()
        .map(|words| strip_gh_repo_flag(words))
        .find_map(|words| statement_reason(role, &words, run))
}

/// The queue chaperone (#2467, DD-192) holds no checkout and never builds:
/// it waits on one queued PR with `pr_merge_watch`, reads CI, reruns a failed
/// run once (`gh run rerun <id> --failed`) and re-enqueues with `pr_merge.py`
/// after a batch-mate's ejection. Everything else is refused.
fn chaperone_reason(words: &[String]) -> Option<String> {
    if creates_issue(words) || closes_issue(words) {
        return Some(format!(
            "{CHAPERONE} never files or closes issues; report in your structured output"
        ));
    }
    if let Some(reason) = remote_ci_reason(CHAPERONE, words) {
        return Some(reason);
    }
    if is_gh_read(words) {
        return None;
    }
    let rerun_failed = program_name(&words[0]) == "gh"
        && words.get(1).is_some_and(|w| w == "run")
        && words.get(2).is_some_and(|w| w == "rerun")
        && words.iter().any(|w| w == "--failed");
    if rerun_failed {
        return None;
    }
    let tool = bundled_tool(words);
    if tool.is_some_and(|t| {
        t.starts_with("github/pr_merge_watch")
            || t == "github/pr_merge.py"
            || t == "github/ci_failure_report.py"
    }) {
        return None;
    }
    Some(format!(
        "{CHAPERONE} may only watch (`github/pr_merge_watch.py --until merged`), read CI \
         (`gh pr|run view|list|checks`, `github/ci_failure_report.py`), rerun failed jobs \
         (`gh run rerun <id> --failed`) and re-enqueue (`github/pr_merge.py`); it has no \
         checkout and never builds, pushes or merges directly"
    ))
}

/// Unlike a planner, the run orchestrator never fetches or creates a worktree.
/// Keep redirections and command-local environment overrides visible: the
/// shared statement view deliberately omits those for other roles.
pub(super) fn orchestrator_shell_reason(command: &str) -> Option<String> {
    use crate::shell_ast::{parse, BaseEnv, Dialect};
    let script = parse(command, Dialect::Posix, &BaseEnv::default());
    let invalid = script.has_error
        || script.depth_capped
        || !script.constructs.is_empty()
        || script.invocations.is_empty();
    if invalid {
        return Some(format!("{ORCHESTRATOR} requires simple read-only commands"));
    }
    for invocation in script.invocations {
        if !invocation.env.is_empty()
            || invocation.redirects.iter().any(|r| r.writes_file())
            || invocation.words.iter().any(|w| w.is_unknown())
        {
            return Some(format!(
                "{ORCHESTRATOR} may not write files or override command environment"
            ));
        }
        let words: Vec<String> = invocation
            .words
            .iter()
            .map(|w| w.text().to_string())
            .collect();
        if !orchestrator_read(&strip_gh_repo_flag(&words)) {
            return Some(format!(
                "{ORCHESTRATOR} permits read-only inspection, not `{}`",
                words.join(" ")
            ));
        }
    }
    None
}

fn orchestrator_read(words: &[String]) -> bool {
    let Some(program) = words.first() else {
        return false;
    };
    let inspection = [
        "ls", "cat", "head", "tail", "wc", "grep", "pwd", "echo", "test", "true",
    ];
    if inspection.contains(&program.as_str()) && inspect_ok(program, words) {
        return true;
    }
    if program == "gh" {
        return orchestrator_gh_read(words);
    }
    if program != "git" {
        return false;
    }
    let Some(sub) = git_subcommand(words) else {
        return false;
    };
    let forbidden = [
        "--output",
        "--ext-diff",
        "--textconv",
        "--filters",
        "--upload-pack",
        "--exec",
    ];
    if sub.iter().any(|w| {
        let option = w.split('=').next().unwrap_or(w);
        option.len() > 2
            && forbidden
                .iter()
                .any(|flag| option.starts_with(flag) || flag.starts_with(option))
    }) {
        return false;
    }
    match sub.first().map(String::as_str) {
        Some("fetch") | None => false,
        Some("ls-remote") => !sub
            .iter()
            .any(|word| word.starts_with("-u") || word.contains("::")),
        Some("worktree") => sub.get(1).is_some_and(|w| w == "list"),
        _ => is_git_read(&sub),
    }
}

fn orchestrator_gh_read(words: &[String]) -> bool {
    if words.iter().any(|w| w == "--watch" || w == "--web") {
        return false;
    }
    if words.get(1).is_some_and(|w| w == "api") {
        return words[2..].iter().all(|w| {
            !w.starts_with("-f")
                && !w.starts_with("-F")
                && !w.starts_with("--field")
                && !w.starts_with("--raw-field")
                && !w.starts_with("--input")
                && !w.starts_with("--method")
                && !w.starts_with("-X")
                && w != "graphql"
        });
    }
    is_gh_read(words)
        || (words.get(1).is_some_and(|w| w == "release")
            && words.get(2).is_some_and(|w| w == "view" || w == "list"))
}

/// Markers in a commit message, PR title or body that make GitHub skip every
/// workflow for the commit (compared case-insensitively).
const SKIP_CI_MARKERS: &[&str] = &[
    "[skip ci]",
    "[ci skip]",
    "[no ci]",
    "[skip actions]",
    "[actions skip]",
    "skip-checks: true",
    "skip-checks:true",
];

/// Labels that add a CI lane; removing one would reduce remote testing.
const CI_LANE_LABELS: &[&str] = &["ci-test", "ci-full", "ci:full", "ci-windows"];

/// Grind only ever adds remote testing (#1429): a command that would
/// suppress remote CI is refused for every grind role. The "skip CI" choice
/// in a run controls local `act` only, so a marker in a commit or PR text, a
/// removed CI-lane label or a disabled workflow is always the model
/// misreading it. Adding a lane label stays allowed.
fn remote_ci_reason(role: &str, words: &[String]) -> Option<String> {
    let program = program_name(&words[0]);
    let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let has = |word: &str| lower.iter().skip(1).any(|w| w == word);
    let marker = lower
        .iter()
        .skip(1)
        .find_map(|w| SKIP_CI_MARKERS.iter().find(|m| w.contains(**m)));
    let rule = |what: &str| {
        Some(format!(
            "{role} may not {what}: grind only adds remote CI, never removes or skips it. \
             The run's CI choice controls local `act` only (#1429)"
        ))
    };
    match program.as_str() {
        "git" => {
            let rewrites = [
                "commit",
                "rebase",
                "merge",
                "tag",
                "revert",
                "cherry-pick",
                "notes",
            ]
            .iter()
            .any(|sub| has(sub));
            match marker {
                Some(m) if rewrites => rule(&format!("put `{m}` in a git message")),
                _ => None,
            }
        }
        "gh" => {
            let pr = |sub: &str| has("pr") && has(sub);
            if let Some(m) = marker {
                if pr("create") || pr("edit") || pr("merge") {
                    return rule(&format!("put `{m}` in a PR title, body or merge message"));
                }
            }
            if pr("edit") {
                let removed = lower.iter().enumerate().any(|(i, w)| {
                    let value = if let Some(v) = w.strip_prefix("--remove-label=") {
                        Some(v)
                    } else if w == "--remove-label" {
                        lower.get(i + 1).map(String::as_str)
                    } else {
                        None
                    };
                    value.is_some_and(|v| {
                        v.split(',')
                            .any(|label| CI_LANE_LABELS.contains(&label.trim()))
                    })
                });
                if removed {
                    return rule("remove a CI-lane label");
                }
            }
            if has("workflow") && has("disable") {
                return rule("disable a workflow");
            }
            None
        }
        _ => None,
    }
}

/// #1339 item 8: the lander and integrator wait on CI only through
/// `pr_merge_watch`, which judges the newest run per workflow and has a
/// deadline; `gh run watch` and `gh pr checks --watch` have neither.
fn native_watch_reason(role: &str, words: &[String]) -> Option<String> {
    if !matches!(role, LANDER | INTEGRATOR) || program_name(&words[0]) != "gh" {
        return None;
    }
    let stripped = strip_gh_repo_flag(words);
    let args = &stripped[1..];
    let sub = (
        args.first().map(String::as_str),
        args.get(1).map(String::as_str),
    );
    let watch_flag = args
        .iter()
        .any(|w| w == "--watch" || w.starts_with("--watch="));
    let watcher = match sub {
        (Some("run"), Some("watch")) => "gh run watch",
        (Some("pr"), Some("checks")) if watch_flag => "gh pr checks --watch",
        (Some("run"), Some("view")) if watch_flag => "gh run view --watch",
        _ => return None,
    };
    Some(format!(
        "{role} may not run `{watcher}`: wait on CI with \
         `\"$CLUD_EXE\" tool run github/pr_merge_watch.py <PR>` (run_in_background) instead (#1339)"
    ))
}

/// #1339 item 9: a shell `grep -r` or `find` walks ignored trees (`target/`
/// holds gigabytes); the Grep and Glob tools use ripgrep and respect
/// `.gitignore`. A depth-bounded `find` lists a directory and stays open.
fn shell_search_reason(role: &str, program: &str, words: &[String]) -> Option<String> {
    if words[0].contains(['/', '\\']) {
        return None;
    }
    let args = &words[1..];
    let (what, tool) = match program {
        "grep" if args.iter().any(|w| is_recursive_grep_flag(w)) => ("grep -r", "Grep"),
        "find" if !args.iter().any(|w| w == "-maxdepth") => ("find", "Glob"),
        _ => return None,
    };
    Some(format!(
        "{role} may not run `{what}` over the tree: use the {tool} tool, which respects \
         .gitignore, or `git grep` (#1339)"
    ))
}

fn is_recursive_grep_flag(word: &str) -> bool {
    match word.strip_prefix("--") {
        Some(long) => {
            matches!(
                long,
                "recursive" | "dereference-recursive" | "directories=recurse"
            )
        }
        None => word.len() > 1 && word.starts_with('-') && word[1..].contains(['r', 'R']),
    }
}

/// Whether a command is a shell loop that runs the repo's lint or test script
/// (`for i in 1 2 3; do bash test; done`, `until ./test; do :; done`), which
/// reruns a deterministic failure instead of reading it (#1425). The flat
/// scanner splits the loop into statements, so a loop keyword heading one
/// statement plus a script in any statement is enough. A wait loop on a
/// marker file (`until [ -f test.exit ]; do sleep 5; done`) runs no script.
fn is_script_retry_loop(statements: &[Vec<String>]) -> bool {
    let looped = statements.iter().any(|words| {
        words
            .first()
            .is_some_and(|w| matches!(w.as_str(), "for" | "while" | "until"))
    });
    looped
        && statements
            .iter()
            .any(|words| runs_repo_script(words.as_slice()))
}

/// The denial reason for a non-shell tool call from `role`, or `None` to
/// allow. No role may call `AskUserQuestion`: /grind asks every question up
/// front, in one round before prework. The planner never writes files, in
/// either phase, and neither does the prework agent.
pub(super) fn tool_reason(role: &str, tool_name: &str, run: &RunFacts) -> Option<String> {
    if role == GATE && !matches!(tool_name, "Bash" | "StructuredOutput") {
        return Some(format!(
            "{GATE} may only execute its bound check and report the result"
        ));
    }
    // Codex reports its file edits as `apply_patch` even where a matcher
    // reaches them through the `Edit`/`Write` aliases (#1954).
    const WRITE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit", "apply_patch"];
    if tool_name.eq_ignore_ascii_case("AskUserQuestion") {
        return Some(format!(
            "{role} may not ask the user: /grind asks every question up front, in one round \
             before prework; follow the rule recorded in the plan instead"
        ));
    }
    if !matches!(role, PLANNER | PREWORK | ORCHESTRATOR)
        || !WRITE_TOOLS
            .iter()
            .any(|t| t.eq_ignore_ascii_case(tool_name))
    {
        return None;
    }
    let mode = if run.plan_only && role == PLANNER {
        " (plan-only mode)"
    } else {
        ""
    };
    Some(format!("{role} may not write files{mode}"))
}

#[expect(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn statement_reason(role: &str, words: &[String], run: &RunFacts) -> Option<String> {
    let program = program_name(&words[0]);
    let deny = |what: &str| Some(format!("{role} may not run `{what}`"));
    if let Some(reason) = feature_reason(role, words, run) {
        return Some(reason);
    }
    if creates_issue(words) {
        return Some(format!(
            "{role} may not run `gh issue create`: the /grind router files problems; report \
             them in your structured output instead"
        ));
    }
    if closes_issue(words) {
        return Some(
            "grind roles never close issues; the feature PR or `clud grind reconcile` closes \
             them (#1393)"
                .to_string(),
        );
    }
    if let Some(reason) = remote_ci_reason(role, words)
        .or_else(|| native_watch_reason(role, words))
        .or_else(|| shell_search_reason(role, &program, words))
    {
        return Some(reason);
    }
    // #1339: the read-only git state and code outline are open to every role.
    if bundled_tool(words).is_some_and(|t| READ_ONLY_TOOLS.contains(&t)) {
        return None;
    }
    // #1339 item 6: every role may search and preview with ast-grep; only
    // the integrator applies a rewrite.
    if bundled_tool(words) == Some("code/ast_grep.py") {
        return (role != INTEGRATOR && ast_grep_writes(&words[4..])).then(|| {
            format!(
                "{role} may search and preview with ast-grep, but not apply a rewrite \
                 (`-U`, `--update-all`, `-i`, `new`); report the rewrite for the integrator (#1339)"
            )
        });
    }
    match role {
        PLANNER => {
            // /grind-plan reads the plan comment through `gh api` (#2426, #1816).
            if inspect_ok(&program, words) || (program == "gh" && orchestrator_gh_read(words)) {
                return None;
            }
            // Recording a task's files (#1340) writes only the run facts.
            if is_clud(&words[0])
                && words.get(1).is_some_and(|w| w == "grind-facts")
                && words.get(2).is_some_and(|w| w == "task")
            {
                return None;
            }
            if program == "git" {
                let Some(sub) = git_subcommand(words) else {
                    return Some(format!(
                        "{role} may not pass git global options other than -C"
                    ));
                };
                return match sub.as_slice() {
                    _ if is_git_read(&sub) => None,
                    [a, b, ..] if a == "worktree" && b == "list" => None,
                    [a, b, ..] if a == "worktree" && b == "add" && run.plan_only => {
                        Some(format!("{role} is in plan-only mode: no worktrees"))
                    }
                    [a, b, ..] if a == "worktree" && b == "add" && run.parallel => None,
                    [a, b, ..] if a == "worktree" && b == "add" => {
                        Some(format!("{role} may create worktrees only in parallel mode"))
                    }
                    _ => deny(&format!("git {}", sub.first().map_or("", String::as_str))),
                };
            }
            deny(&program)
        }
        PREWORK => {
            if inspect_ok(&program, words) || is_gh_read(words) {
                return None;
            }
            // `printf '%s' '<body>' | gh issue comment <meta> --body-file -`:
            // the one producer /grind-prework pipes a plan body from. Exactly
            // a `%s` format and one argument, so no `-v`, and no redirection.
            if program == "printf"
                && !words[0].contains(['/', '\\'])
                && words.len() == 3
                && words[1] == "%s"
            {
                return None;
            }
            if program == "gh"
                && words.get(1).is_some_and(|w| w == "issue")
                && words.get(2).is_some_and(|w| w == "comment")
            {
                return match comment_target(&words[3..]) {
                    Err(flag) => Some(format!("{role} may not pass `{flag}` to gh issue comment")),
                    Ok(Some(target)) if run.is_comment_host(&target) => None,
                    Ok(_) => Some(format!(
                        "{role} may comment only on the meta issue{}",
                        run.meta
                            .as_deref()
                            .map_or(String::new(), |m| format!(" (#{m})"))
                    )),
                };
            }
            if program == "git" {
                let Some(sub) = git_subcommand(words) else {
                    return Some(format!(
                        "{role} may not pass git global options other than -C"
                    ));
                };
                return match sub.as_slice() {
                    _ if is_git_read(&sub) => None,
                    [a, b, ..] if a == "worktree" && b == "list" => None,
                    _ => deny(&format!("git {}", sub.first().map_or("", String::as_str))),
                };
            }
            deny(&program)
        }
        LANDER => {
            if inspect_ok(&program, words) || is_gh_read(words) {
                return None;
            }
            if let Some(verdict) = lander_records_landing(role, words, run) {
                return verdict;
            }
            // pr_merge_watch exit 7 (a stale check): re-run its workflow once.
            if program == "gh"
                && words.get(1).is_some_and(|w| w == "run")
                && words.get(2).is_some_and(|w| w == "rerun")
            {
                return None;
            }
            if program == "gh"
                && words.get(1).is_some_and(|w| w == "pr")
                && words.get(2).is_some_and(|w| {
                    matches!(w.as_str(), "view" | "checks" | "diff" | "list" | "merge")
                })
            {
                return None;
            }
            if program == "gh"
                && words.get(1).is_some_and(|w| w == "pr")
                && words.get(2).is_some_and(|w| w == "ready")
            {
                let target = pr_target(&words[3..]);
                return match run.feature_pr() {
                    Some(pr) if target.as_deref() == Some(pr) => {
                        (run.feature_merge != FeatureMerge::Auto).then(|| {
                            format!(
                                "{role} may ready the feature PR only under feature_merge=auto; \
                                 {} leaves it for the user",
                                policy_name(run.feature_merge)
                            )
                        })
                    }
                    _ => Some(format!(
                        "{role} may run `gh pr ready` only on the feature PR"
                    )),
                };
            }
            if is_clud(&words[0])
                && words.get(1).is_some_and(|w| w == "tool")
                && words.get(2).is_some_and(|w| w == "run")
                && words.get(3).is_some_and(|w| {
                    // pr_merge.py is how /grind-land enqueues a green PR (#2425).
                    w.starts_with("github/pr_merge_watch")
                        || w == "github/pr_merge.py"
                        || w == "github/ci_failure_report.py"
                })
            {
                return None;
            }
            if program == "git" {
                let Some(sub) = git_subcommand(words) else {
                    return Some(format!(
                        "{role} may not pass git global options other than -C"
                    ));
                };
                if is_plain_push(&sub) || is_git_read(&sub) {
                    return None;
                }
                return deny(&format!("git {}", sub.first().map_or("", String::as_str)));
            }
            deny(&program)
        }
        INTEGRATOR => {
            // A denylist is only as good as what it sees, so look through
            // wrappers (`env act`, `xargs bosn`): every word counts. A nested
            // shell's code (`bash -c "docker run …"`) is parsed into
            // statements of its own (#1962).
            let tokens: Vec<String> = words.iter().map(|w| program_name(w)).collect();
            let has = |name: &str| tokens.iter().any(|t| t == name);
            if has("bosn") {
                return deny("bosn");
            }
            if has("docker") || has("podman") {
                return Some(format!(
                    "{role} may not run containers directly; local CI goes through act"
                ));
            }
            if has("act") && !run.ci {
                return Some("local CI (act) is disabled for this grind run".to_string());
            }
            if tokens
                .windows(2)
                .any(|w| w[0] == "worktree" && w[1] == "add")
            {
                return Some(format!("{role} may not create worktrees"));
            }
            None
        }
        // Workers and reviewers hold a shell only to investigate on GitHub:
        // read-only `gh`.
        WORKER | REVIEWER if is_gh_read(words) => None,
        // The patch tool refuses files outside the run facts' task list itself.
        WORKER | REVIEWER if bundled_tool(words) == Some("code/patch.py") => None,
        WORKER | REVIEWER if runs_repo_script(words) => Some(format!(
            "{role} may not run the repo's lint or test scripts; the grind-integrator runs \
             lint/test before every push"
        )),
        WORKER | REVIEWER => Some(format!(
            "{role} may only run read-only `gh` (issue/pr/run view|list, search) and `safe-rm` \
             on literal paths in its task's directories; it cannot build, lint or test"
        )),
        _ => deny(&program),
    }
}

/// A deletion statement a grind role may type (#1340).
enum RmStatement {
    /// `safe-rm` (or `clud safe-rm`): its path operands.
    Direct(Vec<String>),
    /// `find <paths> … -exec safe-rm|safe-rm …`: find's starting paths.
    Find(Vec<String>),
}

fn rm_statement(words: &[String]) -> Option<RmStatement> {
    let tools = crate::deletion_rules::generated().safe_aliases;
    let program = program_name(&words[0]);
    let operands = |args: &[String]| {
        let mut out = Vec::new();
        let mut flags = true;
        for arg in args {
            if flags && arg == "--" {
                flags = false;
            } else if !(flags && arg.starts_with('-')) {
                out.push(arg.clone());
            }
        }
        out
    };
    if tools.contains(&program.as_str()) {
        return Some(RmStatement::Direct(operands(&words[1..])));
    }
    if is_clud(&words[0]) && words.get(1).is_some_and(|w| tools.contains(&w.as_str())) {
        return Some(RmStatement::Direct(operands(&words[2..])));
    }
    if program == "find"
        && words.windows(2).any(|w| {
            matches!(w[0].as_str(), "-exec" | "-execdir")
                && tools.contains(&program_name(&w[1]).as_str())
        })
    {
        let starts = words[1..]
            .iter()
            .take_while(|w| !w.starts_with(['-', '(', '!']))
            .cloned()
            .collect();
        return Some(RmStatement::Find(starts));
    }
    None
}

/// Lexically resolve `.` and `..`, without touching the filesystem.
fn normalize(path: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The per-role deletion roots for `safe-rm` (#1340), checked
/// before the tools run (which enforce the session's roots themselves):
///
/// - a worker or reviewer may delete only under the directories of the
///   files the planner recorded for its checkout (its whole checkout when
///   none were recorded), with literal paths and no `find -exec`;
/// - the integrator may delete only inside its checkout.
///
/// `None` when the command runs neither tool (the role's other rules
/// decide), `Some(None)` to allow a worker's or reviewer's pure deletion,
/// `Some(Some(reason))` to deny. The integrator's other statements still go
/// through its denylist.
pub(super) fn rm_tool_verdict(
    role: &str,
    command: &str,
    run: &RunFacts,
    cwd: &Path,
) -> Option<Option<String>> {
    let profile = match role {
        INTEGRATOR => crate::deletion_rules::Profile::GrindIntegrator,
        WORKER => crate::deletion_rules::Profile::GrindWorker,
        REVIEWER => crate::deletion_rules::Profile::GrindReviewer,
        PLANNER => crate::deletion_rules::Profile::GrindPlanner,
        PREWORK => crate::deletion_rules::Profile::GrindPrework,
        LANDER => crate::deletion_rules::Profile::GrindLander,
        _ => return None,
    };
    let statements = statement_words(command).ok()?;
    let parsed: Vec<Option<RmStatement>> = statements.iter().map(|w| rm_statement(w)).collect();
    if parsed.iter().all(Option::is_none) {
        return None;
    }
    let scope = crate::deletion_rules::scope(profile).ok()?;
    if scope == crate::deletion_rules::DeleteScope::None {
        return Some(Some(format!("{role} may not delete")));
    }
    let cwd = normalize(cwd);
    let checkout = super::nearest_repo_root(&cwd).unwrap_or_else(|| cwd.clone());
    let allowed = match scope {
        crate::deletion_rules::DeleteScope::Checkout => vec![checkout.clone()],
        crate::deletion_rules::DeleteScope::TaskDirs => run
            .task_dirs(&checkout)
            .unwrap_or_else(|| vec![checkout.clone()]),
        crate::deletion_rules::DeleteScope::Session => vec![checkout.clone()],
        crate::deletion_rules::DeleteScope::None => unreachable!(),
    };
    let shown = allowed
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    for statement in &parsed {
        let (operands, is_find) = match statement {
            None if role == INTEGRATOR => continue,
            // A worker's deletion is allowed only on its own.
            None => return None,
            Some(RmStatement::Find(_)) if role != INTEGRATOR => {
                return Some(Some(format!(
                    "{role} may not run `find -exec` deletions; name each path to safe-rm"
                )))
            }
            Some(RmStatement::Find(starts)) => (starts, true),
            Some(RmStatement::Direct(operands)) => (operands, false),
        };
        if operands.is_empty() {
            return Some(Some(format!("{role} must name the paths to delete")));
        }
        for operand in operands {
            // Brace expansion and backslash escapes make bash expand a path
            // other than the one written (`{x,/elsewhere}`, `\.\.`), so they
            // count as dynamic for every role. A glob only matches inside the
            // directory before it, so the integrator may use one.
            let dynamic = operand.contains(['$', '`', '~', '{', '}', '\\']);
            let globbed = operand.contains(['*', '?', '[']);
            if dynamic || (globbed && role != INTEGRATOR) {
                return Some(Some(format!(
                    "{role} may pass only literal paths to safe-rm, not {operand:?}"
                )));
            }
            // A glob is checked by the directory it expands in.
            let literal: std::path::PathBuf = Path::new(operand)
                .components()
                .take_while(|c| !c.as_os_str().to_string_lossy().contains(['*', '?', '[']))
                .collect();
            let path = normalize(&cwd.join(literal));
            // A path must lie strictly inside an allowed directory; a glob or
            // a `find` start may name the directory it searches.
            let ok = allowed
                .iter()
                .any(|dir| path.starts_with(dir) && (path != *dir || globbed || is_find));
            if !ok {
                return Some(Some(format!(
                    "{role} may delete only under {shown}; {operand:?} is outside"
                )));
            }
        }
    }
    (role != INTEGRATOR).then_some(None)
}

fn policy_name(policy: FeatureMerge) -> &'static str {
    match policy {
        FeatureMerge::Auto => "auto",
        FeatureMerge::DecideLater => "decide later",
        FeatureMerge::CommentOnly => "comment only",
    }
}

/// The PR a `gh pr merge|ready` argument list targets: the first non-flag
/// argument, as a number. Value-taking flags skip their value.
fn pr_target(args: &[String]) -> Option<String> {
    pr_positional(args).and_then(|word| number_word(word))
}

/// Whether a `gh pr merge` target names the feature PR: by number, or by its
/// head branch (the feature branch the run facts record). `None` when no target
/// is given, so the caller fails closed.
fn merge_targets_feature(args: &[String], run: &RunFacts, feature_pr: &str) -> Option<bool> {
    let word = pr_positional(args)?;
    if let Some(number) = number_word(word) {
        return Some(number == feature_pr);
    }
    let branch = word.trim().trim_matches(&['\'', '"'][..]);
    let feature_branch = run.feature.as_ref().map(|f| f.branch.as_str());
    Some(feature_branch == Some(branch))
}

/// The first positional argument after `gh pr <sub>`.
fn pr_positional(args: &[String]) -> Option<&String> {
    const VALUE_FLAGS: &[&str] = &[
        "-R",
        "--repo",
        "-b",
        "--body",
        "-F",
        "--body-file",
        "-t",
        "--subject",
        "-A",
        "--author-email",
        "--match-head-commit",
    ];
    let mut i = 0;
    while let Some(word) = args.get(i) {
        i += 1;
        if word.starts_with('-') {
            if VALUE_FLAGS.contains(&word.as_str()) {
                i += 1;
            }
            continue;
        }
        return Some(word);
    }
    None
}

/// Feature-branch-mode rules for every grind role: no `grind/*` branch is
/// deleted while the run facts record a feature (its PR counts as open), and
/// the feature PR merges only as a merge commit, never with `--admin`, and
/// only under `feature_merge=auto`.
fn feature_reason(role: &str, words: &[String], run: &RunFacts) -> Option<String> {
    run.feature.as_ref()?;
    let program = program_name(&words[0]);
    if program == "git" {
        let sub = git_subcommand(words)?;
        let is_grind = |w: &String| w.trim_start_matches(['+', ':']).starts_with("grind/");
        let deletes = match sub.first().map(String::as_str) {
            Some("branch") => {
                sub[1..].iter().any(|w| {
                    w == "--delete"
                        || (w.starts_with('-') && !w.starts_with("--") && w.contains(['d', 'D']))
                }) && sub[1..].iter().any(is_grind)
            }
            Some("push") => {
                (sub[1..].iter().any(|w| w == "--delete" || w == "-d")
                    && sub[1..].iter().any(is_grind))
                    || sub[1..].iter().any(|w| w.starts_with(":grind/"))
            }
            _ => false,
        };
        return deletes.then(|| {
            format!("{role} may not delete grind/* branches while the feature PR is open")
        });
    }
    if program == "gh" && words.get(1).is_some_and(|w| w == "api") {
        let deletes = words
            .iter()
            .any(|w| w.to_ascii_uppercase().ends_with("DELETE"))
            && words.iter().any(|w| w.contains("git/refs/heads/grind/"));
        return deletes.then(|| {
            format!("{role} may not delete grind/* branches while the feature PR is open")
        });
    }
    if program != "gh"
        || words.get(1).is_none_or(|w| w != "pr")
        || words.get(2).is_none_or(|w| w != "merge")
    {
        return None;
    }
    let feature_pr = run.feature_pr()?;
    let args = &words[3..];
    let Some(is_feature) = merge_targets_feature(args, run, feature_pr) else {
        return Some(format!(
            "{role} must name the PR to `gh pr merge` in feature-branch mode"
        ));
    };
    let has = |names: &[&str]| args.iter().any(|w| names.contains(&w.as_str()));
    // Goal PRs head `grind/*` branches too: none is deleted while the
    // feature PR is open.
    if !is_feature {
        return has(&["--delete-branch", "-d"]).then(|| {
            format!("{role} may not delete grind/* branches while the feature PR is open")
        });
    }
    if has(&["--admin"]) {
        return Some(
            "the feature PR may never be merged with --admin; required reviews apply".to_string(),
        );
    }
    if has(&["--delete-branch", "-d"]) {
        return Some(format!(
            "{role} may not delete the feature branch when merging the feature PR"
        ));
    }
    if has(&["--squash", "-s", "--rebase", "-r"]) {
        return Some(
            "the feature PR lands as a merge commit (--merge), not squash or rebase".to_string(),
        );
    }
    if run.feature_merge != FeatureMerge::Auto {
        return Some(format!(
            "{role} may not merge the feature PR: feature_merge is {}, which leaves the PR \
             for the user",
            policy_name(run.feature_merge)
        ));
    }
    (!has(&["--merge", "-m"]))
        .then(|| "the feature PR lands as a merge commit: pass --merge explicitly".to_string())
}

/// Feature mode only: after a goal PR merges into the feature branch, the
/// lander records it so the issue is never lost (#1393 §2): it posts the
/// `grind:v1` marker comment and adds the goal's `Closes #N` line to the
/// feature PR's body. It never creates or applies a label (#1958). `None` when `words` is none of those commands (the lander's other
/// rules then apply); `Some(None)` allows, `Some(Some(reason))` denies.
fn lander_records_landing(role: &str, words: &[String], run: &RunFacts) -> Option<Option<String>> {
    let feature = run.feature.as_ref()?;
    if program_name(&words[0]) != "gh" {
        return None;
    }
    let unquote = |w: &String| w.trim_matches(&['\'', '"'][..]).to_string();
    let sub: Vec<String> = words[1..].iter().map(unquote).collect();
    let verdict = match sub
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["label", ..] | ["issue", "edit", ..] => Some(format!(
            "{role} never creates, applies or removes a label in the target repo (#1958)"
        )),
        ["issue", "comment", ..] => match comment_target(&words[3..]) {
            Err(flag) => Some(format!("{role} may not pass `{flag}` to gh issue comment")),
            Ok(Some(_)) => None,
            Ok(None) => Some(format!("{role} must name one issue to comment on")),
        },
        ["pr", "edit", ..] => {
            let args = &words[3..];
            let target = pr_target(args);
            let only_body = args.iter().enumerate().all(|(i, w)| {
                !w.starts_with('-')
                    || matches!(
                        w.split('=').next().unwrap_or(w),
                        "-b" | "--body" | "-F" | "--body-file" | "-R" | "--repo"
                    )
                    || (i > 0 && matches!(args[i - 1].as_str(), "-b" | "--body"))
            });
            match (target.as_deref(), feature.pr.as_deref()) {
                (Some(t), Some(pr)) if t == pr && only_body => None,
                (Some(t), Some(pr)) if t == pr => Some(format!(
                    "{role} may change only the feature PR's body (--body/--body-file)"
                )),
                _ => Some(format!("{role} may edit only the feature PR's body")),
            }
        }
        _ => return None,
    };
    Some(verdict)
}

/// Feature-branch-mode caps for callers that are not `grind-*` roles: the
/// main-session `/grind` router, which the hook cannot tell apart from any
/// other session, so they apply only while the run facts record a feature
/// (#1392 §6, #1393 §1). The router keeps exactly one worktree (the feature
/// worktree), never closes an issue (the feature PR and `clud grind
/// reconcile` do), never merges the feature PR (the lander does, under
/// `auto`), and never deletes a `grind/*` branch while the feature PR is open.
pub(super) fn router_reason(command: &str, run: &RunFacts) -> Option<String> {
    let feature = run.feature.as_ref()?;
    let stale = "; if no /grind run is active, run `clud grind-facts clear`";
    let statements = statement_words(command).ok()?;
    statements.iter().find_map(|words| {
        let words = &strip_gh_repo_flag(words);
        if closes_issue(words) {
            return Some(format!(
                "the /grind router never closes issues in feature-branch mode: the feature PR's \
                 Closes lines and `clud grind reconcile` do (#1393){stale}"
            ));
        }
        if program_name(&words[0]) == "git" {
            let sub = git_subcommand(words)?;
            if sub.len() >= 2 && sub[0] == "worktree" && sub[1] == "add" {
                let recorded = feature.worktree.as_deref().unwrap_or_default();
                let path = worktree_add_path(&sub[2..]);
                let same = path.as_deref().is_some_and(|p| {
                    let (p, r) = (Path::new(p.trim_end_matches('/')), Path::new(recorded));
                    !recorded.is_empty() && (p.ends_with(r) || r.ends_with(p))
                });
                return (!same).then(|| {
                    format!(
                        "the /grind router keeps exactly one worktree, the feature worktree \
                         ({recorded}); goal worktrees belong to the grind-planner{stale}"
                    )
                });
            }
        }
        if program_name(&words[0]) == "gh"
            && words.get(1).is_some_and(|w| w == "pr")
            && words.get(2).is_some_and(|w| w == "merge")
            && run.feature_pr().is_some()
            && pr_target(&words[3..]).as_deref() == run.feature_pr()
        {
            return Some(format!(
                "the /grind router never merges the feature PR: the grind-lander does under \
                 feature_merge=auto, otherwise the user does{stale}"
            ));
        }
        feature_reason("the /grind router", words, run)
    })
}

/// The `<path>` of `git worktree add [<options>] <path> [<commit-ish>]`.
fn worktree_add_path(args: &[String]) -> Option<String> {
    let mut i = 0;
    while let Some(word) = args.get(i) {
        i += 1;
        if word.starts_with('-') {
            if matches!(word.as_str(), "-b" | "-B" | "--reason") {
                i += 1;
            }
            continue;
        }
        return Some(word.clone());
    }
    None
}

/// A cheap prefilter for [`router_reason`], so an ordinary session's shell
/// calls never read the run facts file.
pub(super) fn may_concern_router(command: &str) -> bool {
    [
        "worktree",
        "close",
        "state=closed",
        "merge",
        "delete",
        "branch",
        ":grind/",
    ]
    .iter()
    .any(|needle| command.contains(needle))
}

/// The issue number a `gh issue comment` argument list targets (`#` stripped,
/// or the trailing number of an issue URL), or `Err(flag)` for any flag other
/// than `-R/--repo`, `-b/--body` and `-F/--body-file` (so `--edit-last`,
/// `--delete-last`, `--editor` and `--web` are refused). Input redirections
/// (`--body-file - <<'EOF'`, `< body.md`) and fd duplications (`2>&1`) are
/// shell plumbing, not arguments; any other output redirection is refused.
fn comment_target(args: &[String]) -> Result<Option<String>, String> {
    let mut target = None;
    let mut i = 0;
    while let Some(word) = args.get(i) {
        i += 1;
        match redirection(word) {
            Some(Redirection::Input { operand_follows }) => {
                if operand_follows {
                    i += 1;
                }
                continue;
            }
            Some(Redirection::FdDup) => continue,
            Some(Redirection::Output) => return Err(word.clone()),
            None => {}
        }
        if word.starts_with('-') {
            let name = word.split('=').next().unwrap_or(word);
            if !matches!(
                name,
                "-R" | "--repo" | "-b" | "--body" | "-F" | "--body-file"
            ) {
                return Err(name.to_string());
            }
            if !word.contains('=') {
                i += 1;
            }
            continue;
        }
        if target.is_some() {
            return Ok(None);
        }
        let bare = word.trim_matches(&['\'', '"'][..]).trim_end_matches('/');
        let tail = bare.rsplit('/').next().unwrap_or(bare);
        let num = tail.strip_prefix('#').unwrap_or(tail);
        if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
            return Ok(None);
        }
        target = Some(num.to_string());
    }
    Ok(target)
}

/// A shell redirection word, as the tokenizer leaves it (quotes stripped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Redirection {
    /// `<file`, `<<EOF`, `<<-EOF`, `<<<text`; `operand_follows` when the word
    /// is the bare operator (`<`, `<<`, `<<<`) and its operand is the next word.
    Input { operand_follows: bool },
    /// `2>&1`, `>&2`: duplicates a descriptor, writes no file.
    FdDup,
    /// Any other `>`: writes a file.
    Output,
}

fn redirection(word: &str) -> Option<Redirection> {
    let op = word.trim_start_matches(|c: char| c.is_ascii_digit());
    if let Some(rest) = op.strip_prefix('<') {
        let operand = rest.trim_start_matches('<').trim_start_matches('-');
        return Some(Redirection::Input {
            operand_follows: operand.is_empty(),
        });
    }
    let rest = op.strip_prefix('>')?;
    let dup = rest
        .strip_prefix('&')
        .is_some_and(|fd| !fd.is_empty() && fd.chars().all(|c| c.is_ascii_digit() || c == '-'));
    Some(if dup {
        Redirection::FdDup
    } else {
        Redirection::Output
    })
}

fn program_name(word: &str) -> String {
    let word = word.trim_matches(&['\'', '"'][..]);
    let bare = word.rsplit(['/', '\\']).next().unwrap_or(word);
    bare.strip_suffix(".exe").unwrap_or(bare).to_string()
}

/// Whether a statement runs a repo `lint`/`test` script in any form: `./lint`,
/// `scripts/test.sh`, `bash lint`, `sh test.sh`, `cmd /c lint.bat`,
/// `powershell -File test.ps1`. The bare `test` builtin (`test -f x`) is not
/// a script: only a path-qualified or extension-bearing name, or a name handed
/// to a shell, counts.
fn runs_repo_script(words: &[String]) -> bool {
    const SHELLS: &[&str] = &[
        "bash",
        "sh",
        "zsh",
        "dash",
        "cmd",
        "powershell",
        "pwsh",
        "call",
        "source",
        ".",
    ];
    let tokens: Vec<&str> = words.iter().map(String::as_str).collect();
    // `Some(qualified)` when `t` names a lint/test script; `qualified` is true
    // for a path or a script extension, which the `test` builtin never has.
    let script_name = |t: &str| -> Option<bool> {
        let bare = t.rsplit(['/', '\\']).next().unwrap_or(t);
        let pathed = bare.len() != t.len();
        let (stem, has_ext) = match bare.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => {
                let ext = ext.to_ascii_lowercase();
                if !matches!(ext.as_str(), "sh" | "bash" | "bat" | "cmd" | "ps1" | "py") {
                    return None;
                }
                (stem, true)
            }
            _ => (bare, false),
        };
        matches!(stem, "lint" | "test").then_some(pathed || has_ext)
    };
    let mut after_shell = false;
    for t in &tokens {
        if let Some(qualified) = script_name(t) {
            if qualified || after_shell {
                return true;
            }
        }
        let program = program_name(t).to_ascii_lowercase();
        if SHELLS.contains(&program.as_str()) {
            after_shell = true;
        } else if after_shell && !t.starts_with(['-', '/']) {
            // A shell's first operand decides; `/c`, `-File`, `-c` do not.
            after_shell = false;
        }
    }
    false
}

/// Whether a statement runs `gh issue create`, looking through wrappers
/// (`timeout 60 gh issue create …`) the way the integrator's denylist does;
/// `bash -c "gh issue create …"` is a statement of its own.
fn creates_issue(words: &[String]) -> bool {
    let tokens: Vec<String> = words.iter().map(|w| program_name(w)).collect();
    strip_gh_repo_flag(&tokens)
        .windows(3)
        .any(|w| w[0] == "gh" && w[1] == "issue" && w[2] == "create")
}

/// Whether a statement closes an issue: `gh issue close …`, or `gh api` on an
/// `…/issues/<n>` path with a `state=closed` field (`-f`, `-F`, `--field`,
/// `--raw-field`, any method). Looks through wrappers like [`creates_issue`].
/// `clud grind reconcile` is a clud invocation, not `gh`, so it is untouched.
fn closes_issue(words: &[String]) -> bool {
    let tokens = strip_gh_repo_flag(words);
    let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let is_gh = |t: &str| program_name(t) == "gh";
    if tokens
        .windows(3)
        .any(|w| is_gh(w[0]) && w[1] == "issue" && w[2] == "close")
    {
        return true;
    }
    let is_issue_path = |t: &str| {
        let t = t.trim_end_matches('/');
        t.split('/').collect::<Vec<_>>().windows(2).any(|p| {
            p[0] == "issues" && !p[1].is_empty() && p[1].chars().all(|c| c.is_ascii_digit())
        })
    };
    let field_closes = |flag: &str, value: Option<&&str>| {
        const FIELD_FLAGS: &[&str] = &["-f", "-F", "--field", "--raw-field"];
        let is_closed = |v: &str| v.trim_matches(&['\'', '"'][..]) == "state=closed";
        if FIELD_FLAGS.contains(&flag) {
            return value.is_some_and(|v| is_closed(v));
        }
        ["--field=", "--raw-field=", "-f", "-F"]
            .iter()
            .any(|p| flag.strip_prefix(p).is_some_and(is_closed))
    };
    tokens.iter().enumerate().any(|(i, t)| {
        if !is_gh(t) || tokens.get(i + 1) != Some(&"api") {
            return false;
        }
        let rest = &tokens[i + 2..];
        rest.iter().any(|t| is_issue_path(t))
            && rest
                .iter()
                .enumerate()
                .any(|(j, t)| field_closes(t, rest.get(j + 1)))
    })
}

/// Drop gh's `-R/--repo <owner/name>` (and `--repo=`/`-R<x>`) from right
/// after each `gh` token, so `gh -R o/r issue view` reads like `gh issue
/// view` to every check here. Without it a planner's plain read was refused,
/// and `gh -R o/r issue close` slipped past the close ban (#1819).
fn strip_gh_repo_flag(tokens: &[String]) -> Vec<String> {
    let unquote = |t: &str| t.trim_matches(&['\'', '"'][..]).to_string();
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        out.push(token.clone());
        i += 1;
        if program_name(&unquote(token)) != "gh" {
            continue;
        }
        while let Some(next) = tokens.get(i).map(|t| unquote(t)) {
            if next == "-R" || next == "--repo" {
                i += 2;
            } else if next.starts_with("--repo=") || (next.starts_with("-R") && next.len() > 2) {
                i += 1;
            } else {
                break;
            }
        }
    }
    out
}

/// Bundled tools that only read, open to every capped role (#1339).
const READ_ONLY_TOOLS: &[&str] = &["git/state.py", "code/outline.py"];

/// Whether ast-grep arguments write files: `-U`/`--update-all` and
/// `-i`/`--interactive` apply rewrites, and `new` scaffolds files.
fn ast_grep_writes(args: &[String]) -> bool {
    args.first().is_some_and(|w| w == "new" || w == "lsp")
        || args.iter().any(|w| match w.strip_prefix("--") {
            Some(long) => long == "update-all" || long == "interactive",
            None => w.starts_with('-') && w[1..].contains(['U', 'i']),
        })
}

/// The tool a `clud tool run <tool>` statement runs.
fn bundled_tool(words: &[String]) -> Option<&str> {
    (is_clud(&words[0])
        && words.get(1).is_some_and(|w| w == "tool")
        && words.get(2).is_some_and(|w| w == "run"))
    .then(|| words.get(3).map(String::as_str))
    .flatten()
}

fn is_clud(word: &str) -> bool {
    matches!(word, "$CLUD_EXE" | "${CLUD_EXE}" | "\"$CLUD_EXE\"") || program_name(word) == "clud"
}

fn is_gh_read(words: &[String]) -> bool {
    program_name(&words[0]) == "gh"
        && matches!(
            (
                words.get(1).map(String::as_str),
                words.get(2).map(String::as_str)
            ),
            (
                Some("issue" | "pr" | "run"),
                Some("view" | "list" | "diff" | "checks")
            ) | (Some("search"), Some(_))
        )
}

/// A read-only git invocation. `branch` and `remote` only in their listing
/// forms: `git branch -D` and `git remote add` mutate the repository.
fn is_git_read(sub: &[String]) -> bool {
    let Some(first) = sub.first() else {
        return false;
    };
    match first.as_str() {
        "branch" => sub[1..].iter().all(|w| {
            matches!(
                w.as_str(),
                "-a" | "-r" | "-v" | "-vv" | "--list" | "--show-current" | "--all" | "--remotes"
            ) || w.starts_with("--contains")
                || w.starts_with("--merged")
                || w.starts_with("--format")
        }),
        "remote" => sub[1..]
            .first()
            .is_none_or(|w| matches!(w.as_str(), "-v" | "show" | "get-url")),
        other => GIT_READ.contains(&other),
    }
}

/// The words after `git` and its `-C <dir>` options, or `None` when any other
/// global option precedes the subcommand. `-c core.sshCommand=…`,
/// `-c alias.x=!cmd`, `--exec-path` and friends run arbitrary programs under
/// an otherwise read-only subcommand.
fn git_subcommand(words: &[String]) -> Option<Vec<String>> {
    let mut i = 1;
    while let Some(word) = words.get(i) {
        if word == "-C" {
            i += 2;
        } else if word.starts_with('-') {
            return None;
        } else {
            break;
        }
    }
    Some(words.get(i..).unwrap_or_default().to_vec())
}

/// `git push [-u] [origin] [<branch>]`: no force, delete, mirror, or refspec
/// that writes somewhere other than the branch of the same name.
fn is_plain_push(sub: &[String]) -> bool {
    sub.first().is_some_and(|s| s == "push")
        && sub[1..].iter().all(|w| {
            matches!(w.as_str(), "-u" | "--set-upstream")
                || (!w.starts_with('-') && !w.contains(':') && !w.starts_with('+'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(parallel: bool, ci: bool) -> RunFacts {
        RunFacts {
            parallel,
            ci,
            ..RunFacts::default()
        }
    }

    #[test]
    fn run_facts_parse_scripts() {
        let facts = RunFacts::from_json(&serde_json::json!({
            "mode": "sequential",
            "scripts": {"lint": "./lint", "test": "bash test"}
        }));
        assert_eq!(facts.scripts.lint.as_deref(), Some("./lint"));
        assert_eq!(facts.scripts.test.as_deref(), Some("bash test"));
        let none = RunFacts::from_json(&serde_json::json!({"mode": "sequential"}));
        assert_eq!(none.scripts, RunScripts::default());
        let invalid = RunFacts::from_json(&serde_json::json!({
            "scripts": {"lint": 5, "test": ""}
        }));
        assert_eq!(invalid.scripts, RunScripts::default());
        let not_object = RunFacts::from_json(&serde_json::json!({"scripts": "lint"}));
        assert_eq!(not_object.scripts, RunScripts::default());
    }

    #[test]
    fn workers_cannot_run_repo_lint_or_test_scripts() {
        let facts = run(true, true);
        for role in [WORKER, REVIEWER] {
            for command in [
                "./lint",
                "./test",
                "bash ./lint",
                "bash lint",
                "bash test",
                "sh test.sh",
                "cmd /c lint.bat",
                "powershell -File test.ps1",
                "pwsh -NoProfile -File ./lint.ps1",
                "bash -c 'bash lint'",
            ] {
                let reason = shell_reason(role, command, &facts)
                    .unwrap_or_else(|| panic!("{role} allowed `{command}`"));
                assert!(
                    reason.contains("integrator") && reason.contains("lint/test"),
                    "{role} `{command}`: {reason}"
                );
            }
        }
        // `./test` is not the `test` builtin, for the inspecting roles either.
        for role in [PLANNER, LANDER] {
            assert!(allowed(role, "test -f Cargo.toml", &facts));
            assert!(!allowed(role, "./test", &facts));
            assert!(!allowed(role, "./lint", &facts));
        }
    }

    #[test]
    fn rm_profile_deletion_roots_follow_the_role() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("repo-wt-1");
        std::fs::create_dir_all(wt.join(".git")).unwrap();
        std::fs::create_dir_all(wt.join("src/cache")).unwrap();
        let facts = RunFacts::from_json(&serde_json::json!({
            "mode": "parallel",
            "tasks": [{"worktree": wt, "files": ["src/cache/mod.rs"]}],
        }));
        assert_eq!(facts.tasks.len(), 1);
        let verdict = |role: &str, command: &str| rm_tool_verdict(role, command, &facts, &wt);

        // A worker deletes only under its task's directories, literally.
        assert_eq!(verdict(WORKER, "safe-rm src/cache/old.rs"), Some(None));
        assert_eq!(
            verdict(REVIEWER, "safe-rm --purge -r src/cache/tmp"),
            Some(None)
        );
        for command in [
            "safe-rm src/main.rs",
            "safe-rm -r src",
            "safe-rm ../other/x",
            "safe-rm src/cache/../../README.md",
            "safe-rm \"$X\"",
            "safe-rm src/cache/*.rs",
            "safe-rm src/cache/{x,../../other}",
            "safe-rm src/cache/\\.\\./\\.\\./README.md",
            "find src -exec safe-rm {} +",
            "safe-rm",
        ] {
            assert!(
                matches!(verdict(WORKER, command), Some(Some(_))),
                "{command}"
            );
        }
        // Mixed with anything else, a worker's usual rules decide.
        assert_eq!(verdict(WORKER, "safe-rm src/cache/a && ls"), None);
        assert_eq!(verdict(WORKER, "ls"), None);

        // With no recorded tasks for this checkout, its whole checkout.
        let untracked = RunFacts::default();
        assert_eq!(
            rm_tool_verdict(WORKER, "safe-rm docs/x.md", &untracked, &wt),
            Some(None)
        );
        assert!(matches!(
            rm_tool_verdict(
                WORKER,
                &format!("safe-rm -r {}", wt.display()),
                &untracked,
                &wt
            ),
            Some(Some(_))
        ));

        // The integrator: anywhere inside its checkout, globs and find too.
        for command in [
            "safe-rm -r target/debug/incremental",
            "safe-rm build/*.o",
            "find . -name '*.tmp' -exec safe-rm {} +",
            "cargo build && safe-rm out.log",
        ] {
            assert_eq!(verdict(INTEGRATOR, command), None, "{command}");
        }
        for command in [
            "safe-rm -r ../repo",
            "safe-rm /etc/hosts",
            "find / -exec safe-rm -r {} +",
            "safe-rm -r {x,/abs/sibling-wt/dir}",
        ] {
            assert!(
                matches!(verdict(INTEGRATOR, command), Some(Some(_))),
                "{command}"
            );
        }
        // Roles without deletion scope refuse the tool.
        assert!(matches!(
            verdict(LANDER, "safe-rm src/cache/a"),
            Some(Some(_))
        ));
        assert!(!allowed(LANDER, "safe-rm src/cache/a", &facts));
        // The planner records tasks, and nothing else through clud.
        assert!(allowed(
            PLANNER,
            "clud grind-facts task --checkout /r -- src/a.rs",
            &facts
        ));
        assert!(!allowed(PLANNER, "clud grind-facts clear", &facts));
    }

    #[test]
    fn integrator_runs_repo_lint_and_test_scripts() {
        let facts = run(false, false);
        for command in [
            "./lint",
            "./test",
            "bash ./lint && bash ./test",
            "sh test.sh",
            "cmd /c lint.bat",
            "powershell -File test.ps1",
        ] {
            assert!(allowed(INTEGRATOR, command, &facts), "{command}");
        }
    }

    fn allowed(role: &str, command: &str, facts: &RunFacts) -> bool {
        shell_reason(role, command, facts).is_none()
    }

    /// #1429: grind only adds remote CI. Markers, removed lane labels and
    /// disabled workflows are refused for every role; adding a lane is not.
    #[test]
    fn grind_roles_may_never_suppress_remote_ci() {
        let facts = run(true, true);
        let refused = [
            r#"git commit -m "feat: x [skip ci]""#,
            r#"git commit --amend -m "feat: x [CI SKIP]""#,
            r#"git commit -m "feat: x" -m "skip-checks: true""#,
            r#"git commit -m "x [no ci]""#,
            r#"git commit -m "x [skip actions]""#,
            r#"git commit -m "x [actions skip]""#,
            r#"gh pr create --title "feat: y [ci skip]" --body b"#,
            r#"gh pr edit 12 --title "y [skip ci]""#,
            r#"gh pr merge 12 --squash --subject "y [skip ci]""#,
            "gh pr edit 12 --remove-label ci-full",
            "gh pr edit 12 --remove-label=ci-test",
            "gh pr edit 12 --remove-label bug,ci:full",
            "gh pr edit 12 --remove-label CI-Windows",
            "gh workflow disable ci.yml",
        ];
        for role in [PLANNER, WORKER, REVIEWER, INTEGRATOR, LANDER, PREWORK] {
            for command in refused {
                let reason = shell_reason(role, command, &facts)
                    .unwrap_or_else(|| panic!("{role} allowed `{command}`"));
                assert!(
                    reason.contains("grind only adds remote CI"),
                    "{role} `{command}`: {reason}"
                );
            }
        }
        for command in [
            "gh pr edit 12 --add-label ci-full",
            "gh pr edit 12 --remove-label bug",
            r#"git commit -m "feat: x""#,
            r#"gh pr create --title "feat: y" --body b"#,
            "gh workflow list",
            r#"git log --grep "[skip ci]""#,
        ] {
            let reason = shell_reason(INTEGRATOR, command, &facts);
            assert!(
                reason.as_deref().is_none_or(|r| !r.contains("remote CI")),
                "`{command}`: {reason:?}"
            );
        }
    }

    /// #1425: a retry loop around the repo's lint/test scripts is refused;
    /// single runs and marker-file wait loops are not.
    #[test]
    fn integrator_may_not_retry_lint_or_test_in_a_loop() {
        let facts = run(false, false);
        for command in [
            "for i in 1 2 3; do bash test; done",
            "for i in 1 2; do bash ./test --integration; done",
            "until bash ./test; do sleep 1; done",
            "while ! ./lint; do true; done",
        ] {
            let reason = shell_reason(INTEGRATOR, command, &facts)
                .unwrap_or_else(|| panic!("integrator allowed `{command}`"));
            assert!(reason.contains("#1425"), "`{command}`: {reason}");
        }
        for command in [
            "bash ./test",
            "bash ./lint && bash ./test --integration",
            "until [ -f test.exit ]; do sleep 5; done",
            "for f in a.rs b.rs; do cat $f; done",
        ] {
            assert!(allowed(INTEGRATOR, command, &facts), "{command}");
        }
    }

    #[test]
    fn gate_without_agent_binding_denies_shell_and_file_tools() {
        let role = "grind-gate";
        assert!(is_grind_role(role));
        let facts = RunFacts::default();
        for command in [
            "gh release view v1 --repo o/r",
            "gh release create v1",
            "true",
        ] {
            assert!(shell_reason(role, command, &facts).is_some(), "{command}");
        }
        for tool in ["Read", "Write", "Edit", "Agent", "AskUserQuestion"] {
            assert!(tool_reason(role, tool, &facts).is_some(), "{tool}");
        }
        let input = serde_json::json!({"subagent_type": role});
        assert!(agent_spawn_reason("Agent", Some(&input), None).is_some());
    }

    /// #2467: the chaperone has no checkout and no build. It watches a queued
    /// PR, reads CI, reruns failed jobs once and re-enqueues; nothing else.
    #[test]
    fn chaperone_watches_reruns_and_reenqueues_only() {
        let facts = run(false, false);
        assert!(is_grind_role(CHAPERONE));
        for command in [
            "clud tool run github/pr_merge_watch.py -- 5 --repo o/r --until merged --timeout 540",
            "\"$CLUD_EXE\" tool run github/pr_merge.py -- 5 --repo o/r --match-head-commit abc",
            "clud tool run github/ci_failure_report.py -- --repo o/r --run 9 --job 3",
            "gh pr view 5 --repo o/r --json state,mergeStateStatus",
            "gh run list --repo o/r --branch main --event merge_group --limit 1",
            "gh run rerun 99 --failed --repo o/r",
        ] {
            assert!(allowed(CHAPERONE, command, &facts), "{command}");
        }
        for command in [
            "gh run rerun 99",
            "gh pr merge 5 --admin --squash",
            "git push origin grind/5",
            "git status",
            "cargo test",
            "bash lint",
            "gh run watch 99",
            "gh issue close 5",
            "safe-rm x",
        ] {
            assert!(!allowed(CHAPERONE, command, &facts), "{command}");
        }
    }

    /// #2467: the lander enqueues with pr_merge.py and reads main's runs for
    /// the red-base comparison.
    #[test]
    fn lander_enqueues_and_compares_with_main() {
        let facts = run(false, false);
        for command in [
            "clud tool run github/pr_merge.py -- 5 --repo o/r --match-head-commit abc",
            "gh run list --repo o/r --branch main --event push --limit 1 --json databaseId,conclusion",
            "gh run view 77 --repo o/r --json jobs",
        ] {
            assert!(allowed(LANDER, command, &facts), "{command}");
        }
    }

    /// #2467 dedup preflight: the planner searches PRs that close a child.
    #[test]
    fn planner_may_search_prs_for_dedup() {
        let facts = run(false, false);
        for command in [
            "gh pr list --repo o/r --state all --search 'Closes #12' --json number,state,title",
            "gh search prs --repo o/r 'Closes #12'",
        ] {
            assert!(allowed(PLANNER, command, &facts), "{command}");
        }
    }

    #[test]
    fn only_bundled_roles_are_capped() {
        assert!(is_grind_role("grind-planner"));
        assert!(is_grind_role("grind-lander"));
        assert!(!is_grind_role("general-purpose"));
        assert!(!is_grind_role("grind-other"));
    }

    #[test]
    fn orchestrator_is_the_capped_router_exception() {
        let role = "grind-orchestrator";
        assert!(is_grind_role(role));
        let input = serde_json::json!({"subagent_type": role});
        assert!(agent_spawn_reason("Agent", Some(&input), None).is_none());
        for internal in [PLANNER, WORKER, REVIEWER, INTEGRATOR, LANDER, PREWORK, GATE] {
            let input = serde_json::json!({"subagent_type": internal});
            assert!(agent_spawn_reason("Agent", Some(&input), None).is_some());
        }
    }

    #[test]
    fn orchestrator_never_writes_or_builds_in_any_run_mode() {
        let role = "grind-orchestrator";
        for parallel in [false, true] {
            let facts = run(parallel, true);
            for command in [
                "git -C /sister switch -c x",
                "git -C /sister fetch origin",
                "git worktree add /new -b x",
                "cargo build",
                "gh issue comment 12 --body changed",
                "gh api repos/o/r/issues -X POST",
                "gh api repos/o/r/issues -f title=changed",
                "git log --output=changed",
                "git cat-file --filters HEAD:path",
                "git cat-file --filte HEAD:path",
                "git cat-file --textc HEAD:path",
                "git ls-remote -u /tmp/helper origin",
                "git ls-remote -u/tmp/helper origin",
                "git ls-remote ext::helper",
                "rg --pre 'cargo build' pattern /sister",
                "LD_PRELOAD=/sister/plugin.so git log",
                "echo changed > file",
                "cat <<'EOF' > file\nchanged\nEOF",
            ] {
                assert!(!allowed(role, command, &facts), "{command}");
            }
            for tool in [
                "Write",
                "Edit",
                "MultiEdit",
                "NotebookEdit",
                "AskUserQuestion",
            ] {
                assert!(tool_reason(role, tool, &facts).is_some(), "{tool}");
            }
        }
    }

    #[test]
    fn orchestrator_inspects_multiple_repositories() {
        let facts = run(false, false);
        for command in [
            "git -C /primary log --oneline -5",
            "git -C /sister ls-remote origin HEAD",
            "git -C /sister worktree list --porcelain",
            "gh issue view 12 --repo o/primary",
            "gh issue view 13 --repo o/sister",
            "gh api repos/o/sister/releases/latest",
            "gh release view v1 --repo o/sister",
            "cat /sister/CLAUDE.md",
        ] {
            assert!(allowed("grind-orchestrator", command, &facts), "{command}");
        }
    }

    #[test]
    fn planner_reads_but_does_not_build() {
        let facts = run(false, false);
        assert!(allowed(PLANNER, "git -C /r log --oneline -5", &facts));
        assert!(allowed(PLANNER, "gh issue view 12 && ls src", &facts));
        assert!(!allowed(PLANNER, "cargo build", &facts));
        assert!(!allowed(PLANNER, "git commit -m x", &facts));
        assert!(allowed(PLANNER, "git branch --show-current", &facts));
        assert!(!allowed(PLANNER, "git branch -D main", &facts));
        assert!(allowed(PLANNER, "git remote -v", &facts));
        assert!(!allowed(
            PLANNER,
            "git remote add x https://e.com/x",
            &facts
        ));
        assert!(!allowed(PLANNER, "ls $(cargo metadata)", &facts));
    }

    #[test]
    fn gh_repo_flag_neither_blocks_reads_nor_hides_writes() {
        let facts = run(false, false);
        let plan = RunFacts {
            plan_only: true,
            ..run(true, false)
        };
        for read in [
            "gh -R zackees/clud issue view 1807 --json body -q .body",
            "gh --repo zackees/clud issue view 1807",
            "gh --repo=zackees/clud pr list",
            "gh -Rzackees/clud issue list",
        ] {
            assert!(allowed(PLANNER, read, &facts), "{read}");
            assert!(allowed(PLANNER, read, &plan), "{read}");
        }
        assert!(!allowed(INTEGRATOR, "gh -R o/r issue close 5", &facts));
        assert!(!allowed(
            INTEGRATOR,
            "gh --repo o/r issue create -t x",
            &facts
        ));
        assert!(!allowed(PLANNER, "gh -R o/r pr merge 5", &facts));
    }

    #[test]
    fn planner_worktrees_only_in_parallel_mode() {
        let add = "git -C /r worktree add /r-wt-1 -b feat/x origin/main";
        assert!(!allowed(PLANNER, add, &run(false, false)));
        assert!(allowed(PLANNER, add, &run(true, false)));
    }

    #[test]
    fn run_facts_parse_plan_phase() {
        let plan = |v: serde_json::Value| RunFacts::from_json(&v).plan_only;
        assert!(plan(
            serde_json::json!({"mode": "parallel", "phase": "plan"})
        ));
        assert!(!plan(serde_json::json!({"mode": "parallel"})));
        assert!(!plan(
            serde_json::json!({"mode": "parallel", "phase": "run"})
        ));
    }

    #[test]
    fn plan_only_planner_cannot_add_worktrees_even_in_parallel() {
        let plan = RunFacts {
            plan_only: true,
            ..run(true, false)
        };
        let add = "git -C /r worktree add /r-wt-1 -b grind/1 origin/main";
        let reason = shell_reason(PLANNER, add, &plan).expect("plan-only add allowed");
        assert!(reason.contains("plan-only"), "{reason}");
        assert!(allowed(PLANNER, add, &run(true, false)));
        assert!(!allowed(PLANNER, "git push origin x", &plan));
        assert!(allowed(PLANNER, "gh issue view 1", &plan));
        assert!(allowed(PLANNER, "git log", &plan));
    }

    #[test]
    fn planner_cannot_write_files() {
        let plan = RunFacts {
            plan_only: true,
            ..run(true, false)
        };
        // `apply_patch` is how Codex writes files (#1954).
        for tool in [
            "Write",
            "Edit",
            "MultiEdit",
            "NotebookEdit",
            "write",
            "apply_patch",
        ] {
            assert!(
                tool_reason(PLANNER, tool, &run(true, false)).is_some(),
                "{tool}"
            );
            let reason = tool_reason(PLANNER, tool, &plan).unwrap();
            assert!(reason.contains("plan-only"), "{reason}");
        }
        assert!(tool_reason(PLANNER, "Read", &plan).is_none());
        assert!(tool_reason(WORKER, "Write", &plan).is_none());
    }

    #[test]
    fn every_grind_role_is_denied_ask_user_question() {
        let plan = RunFacts {
            plan_only: true,
            ..run(true, false)
        };
        for facts in [run(false, false), plan.clone()] {
            for role in [PLANNER, WORKER, REVIEWER, INTEGRATOR, LANDER, PREWORK] {
                for tool in ["AskUserQuestion", "askuserquestion"] {
                    let reason = tool_reason(role, tool, &facts)
                        .unwrap_or_else(|| panic!("{role} allowed {tool}"));
                    // U11 (#1407): the denial names the up-front rule.
                    assert!(reason.contains("up front"), "{role}: {reason}");
                    assert!(reason.contains("before prework"), "{role}: {reason}");
                }
            }
            assert!(tool_reason(WORKER, "Read", &facts).is_none());
            assert!(tool_reason(PLANNER, "Write", &facts).is_some());
        }
    }

    #[test]
    fn workers_and_reviewers_only_investigate_with_gh() {
        for role in [WORKER, REVIEWER] {
            assert!(allowed(
                role,
                "gh issue view 12 --comments",
                &run(true, true)
            ));
            assert!(allowed(
                role,
                "gh search code foo --repo a/b",
                &run(true, true)
            ));
            assert!(!allowed(role, "gh pr merge 5", &run(true, true)));
            assert!(!allowed(role, "cargo test", &run(true, true)));
            assert!(!allowed(role, "ls", &run(true, true)));
        }
    }

    #[test]
    fn integrator_builds_but_skips_bosn_containers_and_worktrees() {
        let facts = run(true, false);
        assert!(allowed(INTEGRATOR, "bash lint && bash test", &facts));
        assert!(allowed(INTEGRATOR, "git push -u origin feat/x", &facts));
        assert!(!allowed(INTEGRATOR, "bosn init", &facts));
        assert!(!allowed(INTEGRATOR, "docker run --rm img", &facts));
        assert!(!allowed(INTEGRATOR, "git worktree add /x", &facts));
    }

    #[test]
    fn every_role_may_read_the_git_state_tool() {
        let facts = run(true, false);
        for role in [PLANNER, WORKER, REVIEWER, INTEGRATOR, LANDER, PREWORK] {
            assert!(
                allowed(role, "clud tool run git/state.py", &facts),
                "{role}"
            );
        }
        for role in [PLANNER, LANDER] {
            assert!(
                !allowed(role, "clud tool run git/other.py", &facts),
                "{role}"
            );
        }
    }

    #[test]
    fn integrator_act_follows_the_ci_answer() {
        let act = "act -W .github/workflows/ci.yml -j test --pull=false";
        assert!(!allowed(INTEGRATOR, act, &run(true, false)));
        assert!(allowed(INTEGRATOR, act, &run(true, true)));
    }

    /// #1339 items 4-5: every role may outline code; workers and reviewers
    /// may patch, and the patch tool enforces the task's file list.
    #[test]
    fn roles_may_outline_and_writers_may_patch() {
        let facts = run(true, false);
        for role in [PLANNER, WORKER, REVIEWER, INTEGRATOR, LANDER, PREWORK] {
            assert!(
                allowed(
                    role,
                    "\"$CLUD_EXE\" tool run code/outline.py src/lib.rs",
                    &facts
                ),
                "{role}"
            );
        }
        let patch = "\"$CLUD_EXE\" tool run code/patch.py <<'EOF'\n=== a.rs\nEOF";
        for role in [WORKER, REVIEWER, INTEGRATOR] {
            assert!(allowed(role, patch, &facts), "{role}");
        }
        for role in [PLANNER, LANDER] {
            assert!(!allowed(role, patch, &facts), "{role}");
        }
    }

    /// #1339 item 6: ast-grep searches for every role, rewrites for the integrator.
    #[test]
    fn only_the_integrator_applies_ast_grep_rewrites() {
        let facts = run(true, false);
        let tool = "\"$CLUD_EXE\" tool run code/ast_grep.py";
        for role in [PLANNER, WORKER, REVIEWER, LANDER, INTEGRATOR] {
            for args in [
                "run -p 'foo($A)' -l rust src",
                "run -p 'f($A)' -r 'g($A)' src",
            ] {
                assert!(
                    allowed(role, &format!("{tool} {args}"), &facts),
                    "{role}: {args}"
                );
            }
        }
        for args in [
            "run -p a -r b -U src",
            "run -p a --update-all",
            "run -i -p a",
            "new rule",
        ] {
            assert!(
                allowed(INTEGRATOR, &format!("{tool} {args}"), &facts),
                "{args}"
            );
            for role in [PLANNER, WORKER, REVIEWER, LANDER] {
                assert!(
                    !allowed(role, &format!("{tool} {args}"), &facts),
                    "{role}: {args}"
                );
            }
        }
    }

    /// #1339 item 9: Grep and Glob, not a shell walk of the tree.
    #[test]
    fn roles_refuse_shell_tree_walks() {
        let facts = run(true, false);
        for role in [PLANNER, REVIEWER, INTEGRATOR, LANDER] {
            for (command, tool) in [
                ("grep -rn foo .", "Grep"),
                ("grep -R foo src", "Grep"),
                ("grep --recursive foo", "Grep"),
                ("find . -name '*.rs'", "Glob"),
            ] {
                let reason = shell_reason(role, command, &facts);
                assert!(
                    reason.as_deref().is_some_and(|r| r.contains(tool)),
                    "{role}: {command}: {reason:?}"
                );
            }
        }
        for command in [
            "grep -n foo src/main.rs",
            "find src -maxdepth 1 -name '*.rs'",
            "git grep -n foo",
            "rg foo src",
        ] {
            assert!(allowed(PLANNER, command, &facts), "{command}");
        }
    }

    /// #1339 item 8: `pr_merge_watch` is the only CI waiter.
    #[test]
    fn lander_and_integrator_refuse_native_ci_watchers() {
        let facts = run(true, false);
        for role in [LANDER, INTEGRATOR] {
            for command in [
                "gh run watch 99",
                "gh pr checks 5 --watch",
                "gh pr checks --watch=true 5",
                "gh -R o/r pr checks 5 --watch",
                "gh run view 99 --watch",
            ] {
                let reason = shell_reason(role, command, &facts);
                assert!(
                    reason
                        .as_deref()
                        .is_some_and(|r| r.contains("pr_merge_watch")),
                    "{role}: {command}: {reason:?}"
                );
            }
        }
        assert!(allowed(LANDER, "gh pr checks 5", &facts));
        assert!(allowed(LANDER, "gh run view 99 --log-failed", &facts));
    }

    #[test]
    fn lander_can_read_bounded_ci_failure_report() {
        let facts = run(true, false);
        assert!(allowed(
            LANDER,
            "clud tool run github/ci_failure_report.py -- --repo owner/repo --run 123 --job 456",
            &facts
        ));
        assert!(!allowed(
            LANDER,
            "clud tool run github/ci_failure_report.py.untrusted -- --repo owner/repo --run 123 --job 456",
            &facts
        ));
    }

    #[test]
    fn lander_watches_and_merges_only() {
        let facts = run(true, false);
        assert!(allowed(
            LANDER,
            "\"$CLUD_EXE\" tool run github/pr_merge_watch.py 5",
            &facts
        ));
        assert!(allowed(LANDER, "gh pr merge 5 --admin --squash", &facts));
        assert!(allowed(LANDER, "gh run view 99 --log-failed", &facts));
        assert!(allowed(LANDER, "gh run rerun 99", &facts));
        assert!(!allowed(LANDER, "gh run cancel 99", &facts));
        assert!(!allowed(LANDER, "cargo test", &facts));
        assert!(!allowed(LANDER, "git commit -am fix", &facts));
    }

    /// #1409: during the bug stage the run facts have no `feature` yet (the router
    /// adds it when the feature stage starts), so the lander merges each bug
    /// PR into `<main>`, whatever the feature merge policy will be.
    #[test]
    fn bug_stage_lander_may_merge_into_main() {
        for policy in ["auto", "later", "comment"] {
            let bugs = RunFacts::from_json(&serde_json::json!({
                "mode": "parallel", "meta": 100, "feature_merge": policy,
                "tracks": {"101": "bug", "102": "feature"}
            }));
            assert!(bugs.feature.is_none(), "{policy}");
            for command in [
                "gh pr merge 101 --merge",
                "gh pr merge 101 --squash --admin",
                "gh pr merge https://github.com/o/r/pull/101 --merge",
            ] {
                assert!(allowed(LANDER, command, &bugs), "{policy}: {command}");
            }
        }
        // Once the feature stage starts, a goal PR still merges (into the
        // feature branch), but the feature PR follows the policy.
        let feature = RunFacts::from_json(&serde_json::json!({
            "mode": "parallel", "meta": 100, "feature_merge": "later",
            "feature": {"branch": "grind/meta-100-1f3a", "pr": 7}
        }));
        assert!(allowed(LANDER, "gh pr merge 102 --merge", &feature));
        assert!(!allowed(LANDER, "gh pr merge 7 --merge", &feature));
    }

    #[test]
    fn no_grind_subagent_can_create_issues() {
        for facts in [run(false, false), run(true, false)] {
            for role in [PLANNER, WORKER, REVIEWER, INTEGRATOR, LANDER, PREWORK] {
                for command in [
                    "gh issue create --title x --body y",
                    "gh issue create --label grind:followup --title x --body y",
                ] {
                    let reason = shell_reason(role, command, &facts)
                        .unwrap_or_else(|| panic!("{role} allowed `{command}`"));
                    assert!(!allowed(role, command, &facts));
                    assert!(
                        reason.contains("the /grind router files problems"),
                        "{role} `{command}`: {reason}"
                    );
                }
            }
        }
        assert!(!allowed(
            INTEGRATOR,
            "bash -c 'gh issue create --title x --body y'",
            &run(true, false)
        ));
    }

    #[test]
    fn no_grind_role_closes_issues() {
        let facts = run(true, false);
        let closes = |role: &str, command: &str| {
            shell_reason(role, command, &facts)
                .is_some_and(|r| r.contains("never close issues") && r.contains("#1393"))
        };
        for role in [INTEGRATOR, LANDER, WORKER, PLANNER, REVIEWER, PREWORK] {
            for command in [
                "gh issue close 12",
                "gh issue close 12 --comment done",
                "gh api repos/o/r/issues/12 -X PATCH -f state=closed",
                "gh api --method PATCH repos/o/r/issues/12 -F state=closed",
                "gh api repos/o/r/issues/12 --field state=closed",
                "gh api repos/o/r/issues/12 --raw-field=state=closed",
                "bash -c 'gh issue close 12'",
            ] {
                assert!(closes(role, command), "{role} `{command}`");
            }
        }
        // Reopening and commenting are not closing.
        for command in [
            "gh issue reopen 12",
            "gh issue comment 12 -b x",
            "gh api repos/o/r/issues/12 -X PATCH -f state=open",
            "clud grind reconcile",
        ] {
            assert!(!closes(LANDER, command), "{command}");
        }
        assert!(allowed(INTEGRATOR, "gh issue reopen 12", &facts));
        assert!(allowed(INTEGRATOR, "gh issue comment 12 -b x", &facts));
        assert!(allowed(INTEGRATOR, "clud grind reconcile", &facts));
        // Only grind roles are capped at all.
        assert!(!is_grind_role("general-purpose"));
    }

    #[test]
    fn inspection_cannot_run_delete_or_hang() {
        let facts = run(true, true);
        for role in [PLANNER, LANDER] {
            assert!(allowed(role, "find src -maxdepth 2 -name '*.rs'", &facts));
            assert!(!allowed(role, "find . -exec cargo build ;", &facts));
            assert!(!allowed(role, "find . -delete", &facts));
            assert!(allowed(role, "tail -n 60 log.txt", &facts));
            assert!(!allowed(role, "tail -f log.txt", &facts));
            assert!(!allowed(role, "tail -nF 5 log.txt", &facts));
        }
    }

    #[test]
    fn git_global_options_other_than_dir_are_refused() {
        let facts = run(true, false);
        assert!(!allowed(PLANNER, "git -c core.sshCommand=sh fetch", &facts));
        assert!(!allowed(PLANNER, "git -c alias.x=!cargo x", &facts));
        assert!(!allowed(PLANNER, "git --exec-path=/tmp log", &facts));
        assert!(!allowed(LANDER, "git -c core.pager=sh log", &facts));
        assert!(allowed(LANDER, "git -C /r status", &facts));
    }

    #[test]
    fn lander_pushes_and_edits_prs_narrowly() {
        let facts = run(true, false);
        assert!(allowed(LANDER, "git push -u origin grind/5-x", &facts));
        assert!(!allowed(
            LANDER,
            "git push --force origin grind/5-x",
            &facts
        ));
        assert!(!allowed(LANDER, "git push origin --delete main", &facts));
        assert!(!allowed(LANDER, "git push origin HEAD:main", &facts));
        assert!(!allowed(LANDER, "git push origin +grind/5-x", &facts));
        assert!(allowed(LANDER, "gh pr checks 5", &facts));
        assert!(!allowed(LANDER, "gh pr close 5", &facts));
        assert!(!allowed(LANDER, "gh pr review 5 --approve", &facts));
    }

    #[test]
    fn integrator_bans_see_through_wrappers() {
        let facts = run(true, false);
        assert!(!allowed(INTEGRATOR, "bash -c 'docker run img'", &facts));
        assert!(!allowed(INTEGRATOR, "env act -j test", &facts));
        assert!(!allowed(INTEGRATOR, "sh -c bosn", &facts));
        assert!(!allowed(
            INTEGRATOR,
            "bash -c 'git worktree add /x'",
            &facts
        ));
        assert!(allowed(INTEGRATOR, "bash -c 'bash lint'", &facts));
    }

    #[test]
    fn each_session_resolves_its_own_runs_facts() {
        use crate::grind_facts::{facts_path_in, lookup_in, Lookup};
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = ("aaaaaaaa-0000", "bbbbbbbb-0000");
        std::fs::write(
            facts_path_in(dir.path(), a).unwrap(),
            r#"{"mode":"parallel","ci":true}"#,
        )
        .unwrap();
        std::fs::write(
            facts_path_in(dir.path(), b).unwrap(),
            r#"{"mode":"sequential","ci":false}"#,
        )
        .unwrap();
        let now = std::time::SystemTime::now();
        let facts = |id: &str| RunFacts::from_lookup(&lookup_in(dir.path(), Some(id), now));
        assert_eq!(facts(a), (run(true, true), None));
        assert_eq!(facts(b), (run(false, false), None));
        // The planner of run A may add worktrees; run B's may not.
        assert!(allowed(PLANNER, "git worktree add ../wt -b x", &facts(a).0));
        assert!(!allowed(
            PLANNER,
            "git worktree add ../wt -b x",
            &facts(b).0
        ));
        // Run B finishing leaves run A's caps untouched.
        std::fs::remove_file(facts_path_in(dir.path(), b).unwrap()).unwrap();
        assert_eq!(facts(a).0, run(true, true));
        let (strict, warning) = facts(b);
        assert_eq!(strict, RunFacts::default());
        assert!(warning.unwrap().contains("strictest"));
        let (strict, warning) = RunFacts::from_lookup(&Lookup::NoSession);
        assert_eq!(strict, RunFacts::default());
        assert!(warning.is_some());
    }

    #[test]
    fn agent_calls_for_grind_roles_are_refused_unless_opted_in() {
        let call = |ty: &str| serde_json::json!({"subagent_type": ty, "prompt": "x"});
        let worker = call("grind-worker");
        assert!(agent_spawn_reason("Agent", Some(&worker), None).is_some());
        assert!(agent_spawn_reason("Agent", Some(&worker), Some("0")).is_some());
        assert!(agent_spawn_reason("Agent", Some(&worker), Some("1")).is_none());
        let explore = call("Explore");
        assert!(agent_spawn_reason("Agent", Some(&explore), None).is_none());
        // Only the Agent tool: a Bash call that mentions a role is not a spawn.
        assert!(agent_spawn_reason("Bash", Some(&worker), None).is_none());
        let reason = agent_spawn_reason("Agent", Some(&call("grind-lander")), None).unwrap();
        assert!(reason.contains("grind-run") && reason.contains(ALLOW_GRIND_AGENTS_ENV));
    }

    #[test]
    fn codex_spawn_agent_calls_for_grind_roles_are_refused() {
        // #1954: Codex reports `spawn_agent` and names the role `agent_type`.
        let worker = serde_json::json!({"agent_type": "grind-worker", "message": "x"});
        assert!(agent_spawn_reason("spawn_agent", Some(&worker), None).is_some());
        assert!(agent_spawn_reason("spawn_agent", Some(&worker), Some("1")).is_none());
        let explorer = serde_json::json!({"agent_type": "explorer", "message": "x"});
        assert!(agent_spawn_reason("spawn_agent", Some(&explorer), None).is_none());
    }

    #[test]
    fn prework_is_a_capped_role() {
        assert!(is_grind_role("grind-prework"));
        assert!(is_grind_role(PREWORK));
    }

    #[test]
    fn prework_comments_only_on_the_meta_issue() {
        let facts = RunFacts {
            meta: Some("100".to_string()),
            ..run(false, false)
        };
        for command in [
            "gh issue comment 100 --body x",
            "gh issue comment '#100' -R o/r --body-file p.md",
            "gh issue comment https://github.com/o/r/issues/100 -b x",
            "gh issue view 7",
            "git log -1",
        ] {
            assert!(allowed(PREWORK, command, &facts), "{command}");
        }
        let reason = shell_reason(PREWORK, "gh issue comment 101 --body x", &facts).unwrap();
        assert!(reason.contains("only on the meta issue"), "{reason}");
        for command in [
            "gh issue comment 100 --edit-last --body x",
            "gh issue create --title t",
            "gh issue edit 100 --title t",
            "gh api -X POST repos/o/r/issues/100/comments",
            "git worktree add ../x",
            "git commit -m x",
            "git push origin x",
            "cargo build",
        ] {
            assert!(!allowed(PREWORK, command, &facts), "{command}");
        }
        let reason =
            shell_reason(PREWORK, "gh issue comment 100 --body x", &run(false, false)).unwrap();
        assert!(reason.contains("only on the meta issue"), "{reason}");
        let parallel = RunFacts {
            meta: Some("100".to_string()),
            ..run(true, false)
        };
        assert!(!allowed(PREWORK, "git worktree add ../x", &parallel));
    }

    /// The /grind-prework skill pipes the plan body from `printf '%s'` into
    /// `gh issue comment <meta> --body-file -`. Stdin redirections (a `<`
    /// file, a heredoc) are plumbing, not a second comment target; output
    /// redirection still writes a file, and `printf` is allowed only as that
    /// bare `%s` producer.
    #[test]
    fn prework_posts_the_plan_through_stdin() {
        let facts = RunFacts {
            meta: Some("100".to_string()),
            ..run(false, false)
        };
        let body = "'<!-- grind:v1 plan run=r1 -->\n~~~json\n{\n \"meta\": 100\n}\n~~~'";
        let piped = format!("printf '%s' {body} | gh issue comment 100 --repo o/r --body-file -");
        let elsewhere = format!("printf '%s' {body} | gh issue comment 101 --body-file -");
        for command in [
            piped.as_str(),
            "gh issue comment 100 --body-file - << 'EOF'\nbody\nEOF",
            "gh issue comment 100 -F - < plan-part-1.md",
            "gh issue comment 100 --body x 2>&1",
        ] {
            assert!(allowed(PREWORK, command, &facts), "{command}");
        }
        for command in [
            elsewhere.as_str(),
            "gh issue comment 101 --body-file - <<'EOF'\nbody\nEOF",
            "gh issue comment 100 --body x > out.txt",
            "gh issue comment 100 --body x >> out.txt",
            "printf '%s' x > plan.md",
            "printf -v PATH '%s' /tmp",
            "printf '%s' a b",
            "./printf '%s' x",
        ] {
            assert!(!allowed(PREWORK, command, &facts), "{command}");
        }
        // Only prework may run printf at all.
        assert!(!allowed(WORKER, "printf '%s' x", &facts));
    }

    /// #2468: in a multi-repository run each sister lane's prework posts on
    /// its own repository's meta, even when the hook's `cwd` resolves to the
    /// session repository's entry (or to none).
    #[test]
    fn multi_repo_prework_comments_on_any_lane_meta() {
        let file = serde_json::json!({"repos": [
            {"repo": "/w/clud", "meta": 2233},
            {"repo": "/w/running-process", "meta": "#1361"},
            {"repo": "/w/zccache", "meta": 2033}
        ]});
        let facts = RunFacts {
            meta: Some("2233".to_string()),
            lane_metas: lane_metas(&file),
            ..run(false, false)
        };
        for command in [
            "gh issue comment 2233 --body x",
            "gh issue comment 1361 --repo o/running-process --body-file - < p.md",
            "gh issue comment 2033 -R o/zccache --body x",
        ] {
            assert!(allowed(PREWORK, command, &facts), "{command}");
        }
        assert!(!allowed(PREWORK, "gh issue comment 999 --body x", &facts));
        let unmatched = RunFacts {
            lane_metas: lane_metas(&file),
            ..run(false, false)
        };
        assert!(allowed(
            PREWORK,
            "gh issue comment 1361 --body x",
            &unmatched
        ));
        assert!(lane_metas(&serde_json::json!({"meta": 5})).is_empty());
    }

    #[test]
    fn prework_may_not_write_files() {
        let facts = run(false, false);
        assert!(tool_reason(PREWORK, "Write", &facts).is_some());
        assert!(tool_reason(PREWORK, "Edit", &facts).is_some());
        assert!(tool_reason(PREWORK, "Read", &facts).is_none());
        assert!(tool_reason(PREWORK, "AskUserQuestion", &facts).is_some());
    }

    #[test]
    fn run_facts_parse_meta() {
        let meta = |v: serde_json::Value| RunFacts::from_json(&v).meta;
        assert_eq!(
            meta(serde_json::json!({"meta": 100})).as_deref(),
            Some("100")
        );
        assert_eq!(
            meta(serde_json::json!({"meta": "#100"})).as_deref(),
            Some("100")
        );
        assert_eq!(meta(serde_json::json!({"meta": ""})), None);
        assert_eq!(meta(serde_json::json!({"mode": "sequential"})), None);
    }

    fn feature_run(policy: FeatureMerge) -> RunFacts {
        RunFacts {
            feature: Some(FeatureFacts {
                branch: "grind/meta-100-r1".to_string(),
                pr: Some("9".to_string()),
                worktree: Some(".clud/grind/worktrees/feature".to_string()),
            }),
            feature_merge: policy,
            ..run(true, false)
        }
    }

    const POLICIES: [FeatureMerge; 3] = [
        FeatureMerge::Auto,
        FeatureMerge::DecideLater,
        FeatureMerge::CommentOnly,
    ];

    #[test]
    fn run_facts_parse_feature_and_policy() {
        let pr = |p: serde_json::Value| {
            RunFacts::from_json(&serde_json::json!({
                "feature": {"branch": "grind/meta-1-x", "pr": p, "worktree": "wt"}
            }))
            .feature
            .unwrap()
            .pr
        };
        assert_eq!(pr(serde_json::json!(123)).as_deref(), Some("123"));
        assert_eq!(pr(serde_json::json!("#123")).as_deref(), Some("123"));
        assert_eq!(
            pr(serde_json::json!("https://github.com/o/r/pull/123")).as_deref(),
            Some("123")
        );
        let facts = RunFacts::from_json(&serde_json::json!({
            "feature": {"branch": "grind/meta-1-x", "pr": 5, "worktree": "wt"}
        }));
        let feature = facts.feature.unwrap();
        assert_eq!(feature.branch, "grind/meta-1-x");
        assert_eq!(feature.worktree.as_deref(), Some("wt"));
        assert_eq!(facts.feature_merge, FeatureMerge::DecideLater);
        let none = RunFacts::from_json(&serde_json::json!({"mode": "sequential"}));
        assert_eq!(none.feature, None);
        assert_eq!(none.feature_merge, FeatureMerge::DecideLater);
        let policy = |p: &str| RunFacts::from_json(&serde_json::json!({"feature_merge": p}));
        assert_eq!(policy("auto").feature_merge, FeatureMerge::Auto);
        assert_eq!(policy("later").feature_merge, FeatureMerge::DecideLater);
        assert_eq!(
            policy("decide_later").feature_merge,
            FeatureMerge::DecideLater
        );
        assert_eq!(policy("comment").feature_merge, FeatureMerge::CommentOnly);
        assert_eq!(
            policy("comment_only").feature_merge,
            FeatureMerge::CommentOnly
        );
    }

    #[test]
    fn lander_merges_feature_pr_only_under_auto_with_merge_commit() {
        let auto = feature_run(FeatureMerge::Auto);
        assert!(allowed(LANDER, "gh pr merge 9 --merge", &auto));
        assert!(allowed(
            LANDER,
            "gh pr merge https://github.com/o/r/pull/9 --merge",
            &auto
        ));
        assert!(!allowed(LANDER, "gh pr merge 9 --merge --admin", &auto));
        assert!(!allowed(LANDER, "gh pr merge 9 --squash", &auto));
        assert!(!allowed(LANDER, "gh pr merge 9 --rebase", &auto));
        for policy in [FeatureMerge::DecideLater, FeatureMerge::CommentOnly] {
            let reason = shell_reason(LANDER, "gh pr merge 9 --merge", &feature_run(policy))
                .expect("non-auto feature merge allowed");
            assert!(reason.contains(policy_name(policy)), "{reason}");
        }
        for policy in POLICIES {
            assert!(allowed(
                LANDER,
                "gh pr merge 5 --merge",
                &feature_run(policy)
            ));
            assert!(allowed(
                LANDER,
                "gh pr merge 5 --admin --squash",
                &feature_run(policy)
            ));
        }
    }

    /// Bash reads an unquoted `#` after whitespace as the start of a
    /// comment, so `gh pr merge #9 --admin` runs a bare `gh pr merge`: the
    /// caps judge what runs, not what was typed after the `#` (#1962).
    #[test]
    fn an_unquoted_hash_starts_a_comment_as_in_bash() {
        assert_eq!(
            statement_words("gh pr merge #9 --admin --merge"),
            Ok(vec![vec![
                "gh".to_string(),
                "pr".to_string(),
                "merge".to_string()
            ]])
        );
        assert!(shell_reason(
            LANDER,
            "gh pr merge #9 --admin --merge",
            &feature_run(POLICIES[0])
        )
        .is_some());
    }

    #[test]
    fn feature_pr_admin_is_always_denied() {
        for policy in POLICIES {
            let reason = shell_reason(
                LANDER,
                // Quoted: unquoted, `#9 …` is a shell comment (see
                // `an_unquoted_hash_starts_a_comment_as_in_bash`).
                "gh pr merge '#9' --admin --merge",
                &feature_run(policy),
            )
            .expect("admin feature merge allowed");
            assert!(reason.contains("--admin"), "{reason}");
        }
    }

    #[test]
    fn lander_readies_feature_pr_only_under_auto() {
        assert!(allowed(
            LANDER,
            "gh pr ready 9",
            &feature_run(FeatureMerge::Auto)
        ));
        assert!(!allowed(
            LANDER,
            "gh pr ready 5",
            &feature_run(FeatureMerge::Auto)
        ));
        for policy in [FeatureMerge::DecideLater, FeatureMerge::CommentOnly] {
            assert!(!allowed(LANDER, "gh pr ready 9", &feature_run(policy)));
        }
        assert!(!allowed(LANDER, "gh pr ready 9", &run(true, false)));
    }

    #[test]
    fn grind_branches_are_not_deleted_while_feature_pr_open() {
        let facts = feature_run(FeatureMerge::Auto);
        for role in [LANDER, INTEGRATOR] {
            for command in [
                "git branch -d grind/5-x",
                "git branch -D grind/meta-100-r1",
                "git push origin --delete grind/5-x",
                "git push origin :grind/5-x",
                "gh pr merge 9 --merge --delete-branch",
                "gh pr merge 9 --merge -d",
                "gh pr merge 5 --admin --merge --delete-branch",
            ] {
                assert!(!allowed(role, command, &facts), "{role} `{command}`");
            }
        }
        assert!(allowed(
            INTEGRATOR,
            "git branch -D grind/5-x",
            &run(true, false)
        ));
        assert!(allowed(INTEGRATOR, "git branch -D feat/x", &facts));
        let api = "gh api -X DELETE repos/o/r/git/refs/heads/grind/5-x";
        assert!(!allowed(INTEGRATOR, api, &facts));
        assert!(allowed(INTEGRATOR, api, &run(true, false)));
    }

    #[test]
    fn feature_pr_merge_must_name_the_merge_method() {
        let auto = feature_run(FeatureMerge::Auto);
        let reason = shell_reason(LANDER, "gh pr merge 9", &auto).expect("bare merge allowed");
        assert!(reason.contains("--merge"), "{reason}");
        assert!(allowed(LANDER, "gh pr merge 9 -m", &auto));
        // Goal PRs are not the feature PR: their method is the lander's call.
        assert!(allowed(LANDER, "gh pr merge 5 --admin --squash", &auto));
    }

    #[test]
    fn merge_target_may_be_a_branch_name() {
        let auto = feature_run(FeatureMerge::Auto);
        // A goal PR named by its head branch is not the feature PR.
        assert!(allowed(
            LANDER,
            "gh pr merge grind/goal-101 --admin --squash",
            &auto
        ));
        assert!(!allowed(
            LANDER,
            "gh pr merge grind/goal-101 --merge -d",
            &auto
        ));
        // The feature branch names the feature PR, with all its rules.
        assert!(allowed(
            LANDER,
            "gh pr merge grind/meta-100-r1 --merge",
            &auto
        ));
        assert!(!allowed(
            LANDER,
            "gh pr merge grind/meta-100-r1 --admin --merge",
            &auto
        ));
        assert!(!allowed(
            LANDER,
            "gh pr merge 'grind/meta-100-r1' --squash",
            &auto
        ));
        let later = feature_run(FeatureMerge::DecideLater);
        assert!(!allowed(
            LANDER,
            "gh pr merge grind/meta-100-r1 --merge",
            &later
        ));
        // No target at all still fails closed.
        assert!(!allowed(LANDER, "gh pr merge --merge", &auto));
    }

    #[test]
    fn lander_records_a_landed_goal_in_feature_mode_only() {
        let facts = feature_run(FeatureMerge::DecideLater);
        for command in [
            "gh issue comment 102 --body 'Landed on grind/meta-100-r1 via #5. \
             <!-- grind:v1 feature-pr=#9 branch=grind/meta-100-r1 goal-pr=#5 run=r1 -->'",
            "gh pr edit 9 --body 'Closes #100 Closes #102'",
            "gh pr edit https://github.com/o/r/pull/9 --body-file body.md",
        ] {
            assert!(allowed(LANDER, command, &facts), "{command}");
            assert!(!allowed(LANDER, command, &run(true, false)), "{command}");
        }
        for command in [
            "gh label create grind:on-feature --force",
            "gh issue edit 102 --add-label grind:on-feature",
            "gh label create wontfix",
            "gh issue edit 102 --add-label wontfix",
            "gh issue edit 102 --remove-label grind:on-feature",
            "gh issue edit 102 --title x",
            "gh issue edit 102",
            "gh issue comment 102 --edit-last --body x",
            "gh pr edit 5 --body 'Closes #102'",
            "gh pr edit 9 --base main",
            "gh pr edit 9 --title x",
            "gh issue close 102",
        ] {
            assert!(!allowed(LANDER, command, &facts), "{command}");
        }
    }

    #[test]
    fn router_caps_apply_only_while_a_feature_is_recorded() {
        let facts = feature_run(FeatureMerge::Auto);
        let plain = run(true, false);
        let wt = ".clud/grind/worktrees/feature";
        let denied = |command: &str, needle: &str| {
            let reason = router_reason(command, &facts)
                .unwrap_or_else(|| panic!("router allowed `{command}`"));
            assert!(reason.contains(needle), "`{command}`: {reason}");
            assert!(router_reason(command, &plain).is_none(), "{command}");
        };
        // S3: the router never closes an issue in feature mode.
        for command in [
            "gh issue close 102",
            "gh api repos/o/r/issues/102 -X PATCH -f state=closed",
        ] {
            denied(command, "never closes issues");
        }
        // Exactly one router worktree: the feature one.
        denied(
            "git -C /r worktree add /r-wt-2 -b grind/x origin/main",
            "exactly one worktree",
        );
        for command in [
            format!("git -C /r worktree add /r/{wt} -b grind/meta-100-r1 origin/main"),
            format!("git worktree add -q {wt} grind/meta-100-r1"),
        ] {
            assert_eq!(router_reason(&command, &facts), None, "{command}");
        }
        // The feature PR is merged by the lander (auto) or the user, never the router.
        denied("gh pr merge 9 --merge", "never merges the feature PR");
        // grind/* branches stay while the feature PR is open.
        denied("git push origin --delete grind/102", "grind/*");
        denied("git branch -D grind/meta-100-r1", "grind/*");
        for command in [
            "gh issue reopen 102",
            "gh issue comment 100 --body 'grind result'",
            "gh pr ready 9",
            "clud grind reconcile",
            "git worktree list",
        ] {
            assert_eq!(router_reason(command, &facts), None, "{command}");
        }
        assert!(may_concern_router("gh issue close 1"));
        assert!(may_concern_router("git worktree add x"));
        assert!(!may_concern_router("ls -la"));
        assert!(!may_concern_router("gh issue view 12"));
    }

    #[test]
    fn run_facts_parse_mode_and_ci() {
        let facts = RunFacts::from_json(&serde_json::json!({"mode": "parallel", "ci": true}));
        assert_eq!(facts, run(true, true));
        assert_eq!(
            RunFacts::from_json(&serde_json::json!({"mode": "sequential"})),
            run(false, false)
        );
    }

    #[test]
    fn issue_1897_scoped_reads_and_quoted_plan_body_fit_role_caps() {
        let mut facts = run(false, false);
        facts.meta = Some("10".into());
        for role in [PLANNER, PREWORK, LANDER, WORKER, REVIEWER] {
            for command in [
                "gh issue view 12 --comments --repo fork/project",
                "gh -R fork/project issue view 12 --json comments",
                "gh pr view 34 --repo fork/project --json state",
            ] {
                assert!(allowed(role, command, &facts), "{role}: {command}");
            }
        }
        assert!(allowed(
            PREWORK,
            "printf '%s' 'plan (preserve these parentheses)' | gh issue comment 10 --repo fork/project --body-file -",
            &facts,
        ));
        assert!(!allowed(
            PREWORK,
            "printf '%s' 'plan' | gh issue comment 11 --repo fork/project --body-file -",
            &facts,
        ));
        for role in [PLANNER, PREWORK, WORKER, REVIEWER] {
            assert!(!allowed(
                role,
                "gh pr merge 34 --repo fork/project --squash",
                &facts
            ));
        }
        for role in [PLANNER, PREWORK, LANDER, WORKER, REVIEWER] {
            assert!(!allowed(
                role,
                "gh issue close 12 --repo fork/project",
                &facts
            ));
        }
    }

    /// #2425 / #2426: the lander lands through `pr_merge.py`, and the planner
    /// reads issues, PRs and API GETs, in the shapes the grind skills type.
    #[test]
    fn lander_merges_and_planner_reads_github() {
        let facts = run(true, true);
        for command in [
            "clud tool run github/pr_merge.py -- 12 --repo o/r --match-head-commit abc",
            "\"$CLUD_EXE\" tool run github/pr_merge.py -- 12 --repo o/r --match-head-commit abc",
            "\"$CLUD_EXE\" tool run github/pr_merge_watch.py 12",
        ] {
            assert!(allowed(LANDER, command, &facts), "lander: {command}");
        }
        for command in [
            "gh issue view 2415 --comments",
            "gh issue view 2415 --repo o/r --comments",
            "gh -R o/r issue view 2415",
            "gh --repo o/r issue view 2415 --json body",
            "gh pr view 12 --json title",
            "gh api repos/o/r/issues/2415/comments",
            "gh api repos/o/r/issues/2415 --jq .body",
            "gh api --paginate repos/o/r/issues/1/comments",
        ] {
            assert!(allowed(PLANNER, command, &facts), "planner: {command}");
        }
        for command in [
            "gh api -X POST repos/o/r/issues -f title=x",
            "gh api repos/o/r/issues -f title=x",
            "gh api --method PATCH repos/o/r/issues/1",
            "gh api graphql -f query=x",
            "gh issue edit 1 --body x",
        ] {
            assert!(!allowed(PLANNER, command, &facts), "planner: {command}");
        }
    }
}

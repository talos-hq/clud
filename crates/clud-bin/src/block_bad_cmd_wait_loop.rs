//! zackees/clud#1880: refuse hand-written waits in shell commands.
//!
//! Every hand-written wait invents a completion check on a stand-in (a process
//! list, a word in output, a status string) and nothing forces it to give up.
//! A supported process-session wait or background completion notification
//! tracks the real process exit. Select from verified tool capabilities. See
//! DD-168 in `docs/DESIGN_DECISIONS.md` for the `sleep` threshold.

use super::block_bad_cmd_wait_scan::{
    heredoc_script, inline_script, mentions_gh_status, peel_wrappers, python_sleep_seconds,
    python_wait, script_file, HeredocUse, Peeled, ScriptKind,
};
use super::*;
use crate::shell_ast::{Invocation, Script};

/// Rule id for `CLUD_BAD_CMD_OVERRIDE=hand-written-wait:<reason>`.
pub(super) const WAIT_LOOP_RULE_ID: &str = "hand-written-wait";

/// The replacement every refusal names verbatim.
pub(super) const WAIT_REPLACEMENT: &str = "Never poll. Choose by declared capabilities, not the tool name: an exec_command session via write_stdin (empty input, same session, until exit); run_in_background plus its completion notification; or a bounded foreground waiter with its own deadline (`clud ci`, `bosn ci run --wait`, `pr_merge_watch`). If unsure, probe a harmless local process.";

/// Longest `sleep` allowed outside a loop and not chained to a status poll.
const MAX_STANDALONE_SLEEP_SECS: f64 = 60.0;

/// `Some(seconds)` for a `sleep`/`Start-Sleep` segment; the inner `None`
/// means the duration is not a literal (a variable, `infinity` counts as
/// infinite).
fn sleep_seconds(words: &[String]) -> Option<Option<f64>> {
    let head = program_name(words.first()?);
    match head.as_str() {
        "sleep" => Some(posix_sleep_seconds(&words[1..])),
        "start-sleep" => Some(powershell_sleep_seconds(&words[1..])),
        _ => None,
    }
}

fn posix_sleep_seconds(arguments: &[String]) -> Option<f64> {
    if arguments.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for argument in arguments {
        if argument.eq_ignore_ascii_case("infinity") {
            return Some(f64::INFINITY);
        }
        let (number, scale) = match argument.chars().last()? {
            's' => (&argument[..argument.len() - 1], 1.0),
            'm' => (&argument[..argument.len() - 1], 60.0),
            'h' => (&argument[..argument.len() - 1], 3600.0),
            'd' => (&argument[..argument.len() - 1], 86400.0),
            _ => (argument.as_str(), 1.0),
        };
        total += number.parse::<f64>().ok()? * scale;
    }
    Some(total)
}

fn powershell_sleep_seconds(arguments: &[String]) -> Option<f64> {
    let mut scale = 1.0;
    let mut value = None;
    for argument in arguments {
        match argument.to_ascii_lowercase().as_str() {
            "}" | ")" => {}
            "-milliseconds" | "-m" => scale = 0.001,
            "-seconds" | "-s" => scale = 1.0,
            other => value = Some(other.parse::<f64>().ok()?),
        }
    }
    value.map(|seconds| seconds * scale)
}

/// `tail -f`/`-F`/`--follow` and PowerShell `Get-Content -Wait`. A
/// `timeout`-wrapped follow has `timeout` as its head and never reaches here.
fn follows_output(words: &[String]) -> bool {
    let Some(head) = words.first().map(|word| program_name(word)) else {
        return false;
    };
    let arguments = &words[1..];
    match head.as_str() {
        "tail" => arguments.iter().any(|argument| {
            argument == "--follow"
                || argument.starts_with("--follow=")
                || (argument.starts_with('-')
                    && !argument.starts_with("--")
                    && argument.contains(['f', 'F']))
        }),
        "get-content" | "gc" => arguments
            .iter()
            .any(|argument| argument.eq_ignore_ascii_case("-wait")),
        _ => false,
    }
}

/// Deepest embedded script (a script file run by a heredoc-written script
/// ...) read.
const MAX_SCRIPT_DEPTH: usize = 4;

/// One hand-written wait the scanner found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Finding {
    pub(super) what: &'static str,
    /// It reads or blocks on GitHub PR, check or run state.
    pub(super) poll: bool,
}

/// What a scan may look at beyond the command text.
#[derive(Clone, Copy)]
pub(super) struct ScanContext<'a> {
    /// Resolves a relative script path (`bash ./watch.sh`).
    pub(super) cwd: Option<&'a Path>,
    /// `git.pr_wait_fail_fast`: refuse a bare native watcher outside a loop.
    pub(super) native_gate: bool,
    /// A clud gh-shim session, whose `gh pr checks --watch` is canonical.
    pub(super) shim_active: bool,
}

/// Facts collected over one script's commands.
#[derive(Default)]
struct Facts {
    unbounded: bool,
    bounded: bool,
    sleeps: Vec<Option<f64>>,
    follow: bool,
    watch: bool,
    /// A native PR watcher (`gh pr checks --watch`), canonical or not.
    any_native: bool,
    /// A native watcher that is not the gh-shim's canonical PR watch.
    raw_native: bool,
    /// Some command reads GitHub PR, check or run state.
    gh_read: bool,
}

/// The shim's own `gh pr checks --watch`, routed to the fail-fast waiter.
fn canonical_shimmed_watch(words: &[String], shim_active: bool) -> bool {
    shim_active
        && gh_positionals(words.get(1..).unwrap_or(&[])).starts_with(&["pr", "checks"])
        && words
            .iter()
            .any(|word| word == "--watch" || word.starts_with("--watch="))
}

impl Facts {
    fn new(script: &Script) -> Self {
        use crate::shell_ast::LoopKind;
        Self {
            unbounded: script.loop_kinds().any(is_unbounded_loop),
            bounded: script.loop_kinds().any(|kind| {
                matches!(
                    kind,
                    LoopKind::ForEach | LoopKind::CStyle { infinite: false }
                )
            }),
            ..Self::default()
        }
    }

    /// Note what one command does; its words with wrappers peeled.
    fn record(&mut self, invocation: &Invocation, scan: ScanContext<'_>) -> Peeled {
        // #1972: `timeout`, `nohup`, `env`, `time`, the gate prefix, ...
        // cannot hide the waiter.
        let peeled = peel_wrappers(&rule_words(invocation));
        if let Some(seconds) =
            sleep_seconds(&peeled.words).or_else(|| python_sleep_seconds(&peeled.words))
        {
            self.sleeps.push(seconds);
        }
        // `timeout 60 tail -f` has its own deadline (DD-168).
        self.follow |= !peeled.timed && follows_output(&peeled.words);
        self.watch |= peeled
            .words
            .first()
            .is_some_and(|word| program_name(word) == "watch");
        if native_gh_waiter(&peeled.words) {
            self.any_native = true;
            self.raw_native |= !canonical_shimmed_watch(&peeled.words, scan.shim_active);
        }
        // The status read may sit in a substitution (`p=$(gh pr checks 1)`);
        // the invocations include those.
        self.gh_read |=
            gh_poll_target(&peeled.words) || mentions_gh_status(&peeled.words.join(" "));
        peeled
    }

    fn verdict(&self, native_gate: bool) -> Option<Finding> {
        let looping = self.unbounded || self.bounded;
        let has_sleep = !self.sleeps.is_empty();
        let poll = self.any_native || ((self.unbounded || has_sleep) && self.gh_read);
        let what = if self.any_native && looping {
            "a native PR watcher (`gh pr checks --watch`) retried in a loop"
        } else if self.unbounded {
            "a `while`/`until`/infinite `for` loop"
        } else if self.bounded && has_sleep {
            "a `for` loop that sleeps between iterations"
        } else if self.raw_native && native_gate {
            "a native PR watcher (`gh pr checks --watch` outside a clud gh-shim session)"
        } else if self.follow {
            "`tail -f`/`--follow` without `timeout`"
        } else if self.watch {
            "`watch`"
        } else if poll && has_sleep {
            "a `sleep` chained to a GitHub status poll"
        } else if self
            .sleeps
            .iter()
            .any(|seconds| seconds.is_none_or(|s| s > MAX_STANDALONE_SLEEP_SECS))
        {
            "a `sleep` longer than 60 s, or of non-literal length"
        } else {
            return None;
        };
        Some(Finding { what, poll })
    }
}

/// A script one command runs that the shell grammar cannot see into: a
/// `python -c` payload, a heredoc fed to Python or written to a script
/// file, or a literal script file on disk. (A `bash -c`, `eval` or heredoc
/// fed to a shell is already parsed into the tree.) A written or on-disk
/// script may be a repository's own, so only a PR/CI wait in it counts.
fn embedded_wait(
    invocation: &Invocation,
    peeled: &Peeled,
    scan: ScanContext<'_>,
    depth: usize,
) -> Option<Finding> {
    if let Some((ScriptKind::Python, script)) = inline_script(&peeled.words, ShellDialect::Posix) {
        if let Some(finding) = script_wait(ScriptKind::Python, &script, scan, depth) {
            return Some(finding);
        }
    }
    if let Some((usage, kind, body)) = heredoc_script(invocation) {
        let parsed_already = usage == HeredocUse::Fed && matches!(kind, ScriptKind::Shell(_));
        if !parsed_already {
            let finding = script_wait(kind, &body, scan, depth)
                .filter(|finding| usage == HeredocUse::Fed || finding.poll);
            if finding.is_some() {
                return finding;
            }
        }
    }
    let run_in = invocation.cwd.as_literal().map(Path::new).or(scan.cwd);
    let (kind, script) = script_file(&peeled.words, run_in)?;
    script_wait(kind, &script, scan, depth).filter(|finding| finding.poll)
}

fn script_wait(
    kind: ScriptKind,
    script: &str,
    scan: ScanContext<'_>,
    depth: usize,
) -> Option<Finding> {
    match kind {
        ScriptKind::Shell(dialect) => {
            scan_script(&parse_command(script, dialect, scan.cwd), scan, depth + 1)
        }
        ScriptKind::Python => python_wait(script).map(|poll| Finding {
            what: "a Python loop that sleeps",
            poll,
        }),
    }
}

/// What the script does that counts as a hand-written wait, if anything.
fn scan_script(script: &Script, scan: ScanContext<'_>, depth: usize) -> Option<Finding> {
    if depth > MAX_SCRIPT_DEPTH {
        return None;
    }
    let mut facts = Facts::new(script);
    for invocation in &script.invocations {
        let peeled = facts.record(invocation, scan);
        if let Some(finding) = embedded_wait(invocation, &peeled, scan, depth) {
            return Some(finding);
        }
    }
    facts.verdict(scan.native_gate)
}

/// How to run the blessed PR watcher, named in every PR/CI refusal.
pub(super) const PR_WATCH_USAGE: &str = "For a PR start `clud tool run github/pr_merge_watch.py -- <PR> --repo OWNER/NAME --timeout 540` once. The default scope is branch-required checks; use `--require '.*'` for all reported checks (overrides branch protection), and `--no-cancel` if runs must remain intact. Exit 0/green continues the already-authorized next action only for the selected scope; 1/2 require failure/review diagnosis; 64 means correct the command. It also waits for the merge itself: a PR in a merge queue or set to auto-merge is followed until it is MERGED (exit 0) or ejected (13, naming the failing merge-group job or the removal reason); `--no-merge-wait` stops at green instead. Never poll `gh pr view` for a merge. Exit 4 is incomplete: revalidate the same CI and watch again after this process exits. Exit 124 with status in-progress is a resumable watchdog result: reuse the same args after the wrapper has stopped the process. Never restart a live session. A blocked goal needs the runtime's supported resume action; if none is available, report that user resume is needed rather than claiming it is active. For an Actions run with no PR (workflow_dispatch, schedule, push) start `clud tool run github/run_watch.py -- <RUN_ID> --repo OWNER/NAME` once in the background instead: exit 0 green, 1 on the first failed job (`--cancel` also cancels the run), 124 resumable timeout.";

/// The refusal for a hand-written wait, unless an audited
/// `CLUD_BAD_CMD_OVERRIDE=hand-written-wait:<reason>` accepts it.
pub(super) fn hand_written_wait_reason(
    script: &Script,
    command_text: &str,
    scan: ScanContext<'_>,
    evaluation: &mut CommandEvaluation,
) -> Option<String> {
    let Finding { what, poll } = scan_script(script, scan, 0)?;
    if let Some(override_reason) = accepted_override_reason(WAIT_LOOP_RULE_ID) {
        evaluation.log_messages.push(format!(
            "BAD_CMD_OVERRIDE accepted rule={WAIT_LOOP_RULE_ID} reason={override_reason:?} command={command_text:?}"
        ));
        return None;
    }
    let pr_hint = if poll {
        format!(" {PR_WATCH_USAGE}")
    } else {
        String::new()
    };
    Some(format!(
        "[{WAIT_LOOP_RULE_ID}] Refused {what} (#1880). {WAIT_REPLACEMENT}{pr_hint} Override: real env {BAD_CMD_OVERRIDE_ENV}=\"{WAIT_LOOP_RULE_ID}:<reason>\"."
    ))
}

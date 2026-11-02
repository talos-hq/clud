# Bundled tools

`src/tools.rs` embeds the scripts here and installs them under
`~/.clud/tools/`. Invoke one with `clud tool run <relative-path> [args]`.

The runner returns the tool's own exit code when the tool finishes. Its
watchdog returns `124` when a time limit fires. A resumable tool gets a JSON
line on stderr with `status: in-progress`; run it again with the same args.
A killable tool gets `status: aborted`. If an observer resumes too late to
determine whether a deadline was met, the runner reports `status:
observation-delayed` and `deadline_outcome: unknown`, with the observed child
exit code. A zero child exit then returns wrapper code `124` rather than
claiming a verified success; a nonzero child exit is preserved. Exit `0` means
the tool finished successfully within what the observer could verify. The watchdog passes its command cap to children as
`CLUD_TOOL_COMMAND_TIMEOUT_SECS`; `github/pr_merge_watch.py` uses that value
to finish its own timeout and cleanup before the wrapper's cap.

Stdout over 32 KB truncates visibly with an artifact path; see
[bounded-output.md](../../../../docs/architecture/bounded-output.md).

For a large issue, use `clud tool run github/issue_context.py -- --repo OWNER/REPO --issue N`.
It returns a bounded overview and a private snapshot path. Read saved material with
`--artifact PATH --section body`, `--section comments --offset N`, or
`--section comment:ID --offset N`; offsets for body and comment text count Unicode
characters. `--refresh PATH` fetches a new snapshot and reports added, edited,
and deleted comments. The existing loop issue cache and `/grind` context pack
remain separate; refresh each explicitly. Windows and macOS fail closed until
private artifact ACL verification is available (#2246, #2256).

For a pull request with many reviews or review comments, use `clud tool run
github/pr_context.py -- --repo OWNER/REPO --pr N`. It returns a bounded overview (review
states, check summary, omission counts) plus a private snapshot path. Continue with
`--artifact PATH --section body`, `--section reviews`, `--section review:<id>`,
`--section comments`, `--section comment:<id>`, `--section review-comments` or
`--section checks`; `--refresh PATH` reports what changed. Use it instead of
`gh pr view --json reviews,comments`, which prints every review and thread body at once.

For a failed GitHub Actions job, use
`clud tool run github/ci_failure_report.py -- --repo OWNER/REPO --run RUN_ID --job JOB_ID`.
It reads the completed job even while siblings run, prints bounded possible
failure diagnostics, and saves the raw log privately. Use its printed
`--artifact` continuation to inspect a line range without another GitHub call.
For a very long line, `--byte-offset N --byte-count 80` returns text and exact
hexadecimal bytes for bounded continuation.
Exit 0 means the log was retrieved, not that the job passed. Empty, unfinished,
unavailable or unsaved logs produce a nonzero exit. Windows and macOS currently fail
closed until artifact ACLs can be verified (#2246, #2256); no log is downloaded there.

For noisy local test runs, `clud tool run diagnostics/test_report.py -- -- python -m unittest`
returns a bounded failure-first report and saves combined output privately. It preserves
the command's exit status, marks timeouts and incomplete capture explicitly, and
prints an artifact path. Inspect raw bytes without rerunning with
`clud tool run diagnostics/test_report.py -- --artifact PATH --offset 0 --count 80`.
Routine successful test lines are suppressed; warnings and failure diagnostics are
retained where recognized. Unfamiliar output is classified as unknown. This path
currently fails closed on Windows and macOS until private artifact ACL verification is available.
Interactive `clud test run` is unchanged.

`clud tool run diagnostics/codex_audit.py -- TRANSCRIPT.jsonl` reports a bounded,
privacy-preserving count of Codex/default-harness and Claude tool activity across
turns. It counts text result bytes once per known result event; missing event
identities and excluded binary-containing blocks are reported as uncertainty, so
byte counts can be incomplete or include replayed text. Repeated
reads compare observed output, which does not prove a reread was unnecessary.
Repeated consecutive call pairs can span turns; they identify matching call shapes,
not whether repeating them was unnecessary.
Nested JavaScript tool calls are static mentions, not proof of execution. Wait
states are observations from output, not judgments of whether polling was needed.
The report contains no commands, paths, transcript contents, or session IDs;
byte counts are not provider token counts.

`diagnostics/transcript_report.py` reports repeated tool-call bursts,
compactions, context jumps and context errors from one Claude transcript.
It joins clud's launch-context record by hashed session id to report the
effective max-context value and its source
([launch-plan.md](../../../../docs/architecture/launch-plan.md#launch-context-record-1675)).
Its output holds counts and salted fingerprints, never raw content (#1276,
[DD-138](../../../../docs/DESIGN_DECISIONS.md#dd-138-the-first-1276-slice-is-a-read-only-transcript-analyzer-not-a-repeated-call-guard)).

`code/outline.py` lists a file's definitions (Rust, Python, JS/TS, C/C++,
via pinned tree-sitter wheels) and Markdown headings with line ranges, so
`/grind` agents read one range instead of paging through a file (#1339). `code/patch.py` applies
search/replace blocks across files all-or-nothing, each block matching
exactly once in the file's own line endings (an LF block matches a CRLF
working tree and keeps CRLF); in a `/grind` run it refuses files outside the
run facts' task list for the checkout (#1339). `code/ast_grep.py` runs a pinned
ast-grep for structural search and rewrite (#1339).

PR watcher callers select a declared session/background/foreground wait and
required-check or all-check completion scope; see
[watch recovery](../../../../docs/architecture/hook-dispatch.md#watch-recovery-and-continuation-2004).

`github/pr_merge.py` submits an exact GitHub PR head to an enforced merge
queue, or performs a normal squash merge when no queue applies. It requires
`--repo OWNER/REPO` and `--match-head-commit` with the full head SHA; it never
uses admin bypass. A successful enqueue is not a merged PR: follow it with
`github/pr_merge_watch.py`. See [CI merge queue](../../../../docs/architecture/ci.md#merge-queue-2048-decided).

To get woken when detached work finishes, use `notify/watch.py`. One process
watches N clud sessions and M PRs (`add-session ID`, `add-pr OWNER/REPO#N`) and
appends each transition to `~/.clud/state/watch/events.jsonl`. Transitions are
session sealed, PR queued, merged, ejected, failed, closed, checks timed out,
and errors. Each event has a dedupe `key`, so running the watcher twice records
an event once. Read events back with `since N` (a cursor). `tick` polls and then
prints the last events; its output changes only when a new event lands, so a
Hermes `monitor_script` cron can hash it and start an agent turn only then.
An unreachable daemon, a GitHub failure or missing Telegram credentials each
record an `error` event, at most one per source and subject each hour.

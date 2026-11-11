# Bounded tool output

Owner doc for the bounded-output practice in clud-owned investigation
guidance (bundled skills) and bundled tools (#1676, parent #1276). Skills link
here with one line; they do not copy this text.

## Why

In #1276 the context refilled from many ~1.8 KB tool results repeated hundreds
of times, plus single results up to ~62 KB. Each result stays in the
conversation until compaction, so output size times call count is what fills
the window. The analyzer that measured this is
`crates/clud-bin/assets/tools/diagnostics/transcript_report.py` (DD-138..DD-140).

## The practice

1. **Narrow searches.** Scope by path and file type before searching the
   whole tree; prefer a specific pattern over a broad one.
2. **Cap match and list output.** Limit matches (`rg -m`, `head -n`,
   `--limit`), and ask for counts or file names before full matches.
3. **Read files in ranges.** Read the part you need (offset/limit,
   `sed -n 'a,bp'`), not whole large files or whole logs.
4. **Keep bulk artifacts on disk and summarize.** Write long logs, diffs and
   JSON to a file under the clud tmp dir, then read or grep the file; report a
   summary and the path.
5. **Never repeat an identical call without a new reason.** If the output
   would be the same, use the result you already have. Polling is the
   exception, and it needs a wait between calls.

The repeated-call guard
([hook-dispatch.md](hook-dispatch.md#repeated-call-guard-1674), default
`N = 200` identical calls per response) is the backstop for a runaway loop,
not the practice. A run that relies on it has already spent the context.

## Bundled tools

A bundled tool whose result can exceed 32 KB prints the first 32 KB, then an
explicit notice:

```text
[TRUNCATED: showed 32768 of <n> bytes; full output: <path>]
```

The full text goes to `~/.clud/tmp/tool-output/<tool>-<stamp>-<pid>.txt`
(`USERPROFILE` on Windows; `CLUD_TOOL_OUTPUT_DIR` overrides it), never the
project checkout. If the artifact cannot be written, the notice says
`full output could not be saved (<reason>)` and the tool still truncates.
Output under the cap is printed unchanged. Each tool carries its own copy of
the small `emit_bounded` helper, because bundled tools are installed as
standalone files and cannot import each other.

Audit (#1676):

| Tool | Max plausible stdout | Action |
| --- | --- | --- |
| `python/lint_deadcode.py` | unbounded (one JSON row per finding) | `emit_bounded` |
| `diagnostics/transcript_report.py` | unbounded (`--json` lists every response; text lists every burst) | `emit_bounded` |
| `github/issue_context.py` | bounded issue overview and explicit continuation | private issue snapshot |
| `github/pr_context.py` | bounded pull-request overview and explicit continuation | private pull-request snapshot |
| `github/pr_status.py` | a bounded line per PR, capped at 12 PRs / 3 failing names / 4 KiB | `--limit`, `--checks`, page footer |
| `github/ci_failure_report.py` | failure summary and ranged continuation | bounded summary plus private raw-log artifact |
| `github/pr_merge_watch.py` | a few lines per check, but `first error:` was one unbounded log line | line capped at 1000 chars with a notice pointing at the log probe |
| `git/review_range.py`, `git/ci_targets.py`, `github/is_meta_issue.py`, `github/run_watch.py` | < 2 KB fixed-shape JSON | none |
| `git/clud-git-diff.py` | diff goes to a webview; stdout is a status line | none |
| `docker/*.py` | short status lines; docker build output is streamed live by the child process | none |
| `hooks/*.py` | short hook verdicts | none |

Tests: `tests/test_bounded_tool_output.py`.

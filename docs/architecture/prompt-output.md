# Foreground print output

Ordinary subprocess `clud -p` turns for the Claude and Codex harnesses use
native JSONL events internally and render compact classified activity on stderr.
The complete final answer is written once to stdout, so piping the answer still
works. Interactive/explicit PTY launches, loop rendering, daemon event storage,
and other harnesses retain their respective protocols.

Examples in a Unicode terminal:

```text
💭 [thinking] Thinking
🔧 [tool] Bash: soldr cargo test
🔧 [tool] Bash: 42 output lines
✏ [edit] Edit: src/main.rs
✏ [edit] Edit: src/main.rs (+3/-2 lines; 1 output line)
✅ [done] 2 tool calls, 1 edit
```

Every icon has a meaningful ASCII label. Redirected progress and `TERM=dumb`
omit icons. `LC_ALL`, then `LC_CTYPE`, then `LANG` select UTF-8 versus the ASCII
common subset of legacy encodings; `CLUD_OUTPUT_ASCII=1` forces the latter.
Without a locale, Unix uses ASCII, while Windows checks Windows Terminal or
UTF-8 console capability (redirected Windows streams default to UTF-8).
Non-ASCII text on the legacy path becomes `?`, including paths, diagnostics and
final answers. Invalid child UTF-8 is decoded lossily, then replaced with `?`
on the ASCII path. I/O errors are handled without a formatting panic.

## Classification and counts

Thinking/reasoning content is never printed; only the activity classification
is shown. Tool names and short command/path/query summaries identify work.
Large arguments, tool bodies, and command output are omitted. Completed tool
calls report the number of returned text lines, including blank lines and a
non-newline-terminated final line. A trailing newline does not add a line.
Non-text tool results contribute zero text lines; omitted results are reported
as unavailable. Failures retain their status and a bounded diagnostic.

Claude `Edit`/`MultiEdit`/`Write` completion uses `tool_use_result.structuredPatch`
where present. Only `+` and `-` hunk lines count; unchanged context does not.
Without patch metadata, replacements are applied to the original file and a
Myers line diff compares the full before/after states. This counts multiple
replacements on one line once. Overwrites also compare complete file contents. Missing information is reported as
unavailable, never fabricated as zero. The initial edit line identifies the
request; the completion line reports actual counts.

Codex command completions count `aggregated_output`; MCP calls count text
content. Codex's public `file_change` JSON omits diffs. At EOF clud reads only
the native session log for the announced thread under `CODEX_HOME/sessions`
(or `~/.codex/sessions`), bounded to the last 32 MiB. Successful legacy
`patch_apply_end` records and paginated `FileChange` records supply per-call
patches, in order, restricted to the current turn. File headers and context do
not count; file creation/deletion counts the recorded content. Edits report
counts at turn completion. A call editing multiple files shows their summed
`+added/-removed` counts and a bounded path summary; unavailable/disabled/truncated history is explicitly
identified. Codex `--ephemeral` keeps classification but cannot supply history-based
edit counts. An event carrying a diff directly needs no history lookup.

Explicit native output options (`--output-format`, `--json`, output-file/schema
options) retain native output instead of being overridden.
For example: `clud --claude -p "hello" -- --output-format json` and
`clud --codex -p "hello" -- --json`.

## Validation

`prompt_output`/`prompt_patch` Rust tests cover classification, deduplication,
locale/replacement behavior, patch context, repeated edits and resumed turns.
`tests/integration/test_prompt_output.py` runs the real launcher and mock-agent
for both harnesses. Its file-editing fixture actually writes the after-state;
fixed expected counts are independently checked with Python `difflib` and
against clud's output. It covers context, insertion, deletion, CRLF, missing
final newline, no-op edits, UTF-8 and ASCII final output. Windows coverage uses
`ci-test` because the native console capability adapter and integration path
are part of this contract.

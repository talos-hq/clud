# test_runtime/

Project-local test-runtime histogram (#407). Records what each test bucket
actually costs on *this* checkout so the run-all-vs-targeted choice is made from
data rather than vibes.

Design and rationale: [`docs/architecture/test-runtime-memory.md`](../../../../docs/architecture/test-runtime-memory.md)
(accepted under #405). Every position taken here is argued there; this README is
the per-file inventory.

## Files

- `store.rs` — the append-only JSONL store. `record` appends, `load`/`summarize`
  read, `compact` enforces retention. Key items: `RunRecord` (the on-disk row),
  `BucketStats`, `percentile`, `human_ms`.
- `cli.rs` — `clud test run` (wrapper) and `clud test stats` (read).
  `repo_root_from` walks up for `.git`; `format_summary` renders the table.

## Diagnostic capture

`capture.rs` owns `clud test capture`, used by the bundled
`diagnostics/test_report.py` reporter. Neither layer adds a deadline by default.
An explicit reporter `--timeout` (or native `--timeout-secs`) accepts 1..86400
seconds and retains the timeout result and tree-stop behavior.

The private artifact retains at most 1 GiB. At the cap or an artifact write
failure, capture stops retaining bytes and continues draining the merged pipes
until child exit and EOF, or an explicit timeout/cancellation. Reports mark
incomplete evidence with the reason; an incomplete zero exit cannot pass, while
an observed failure or signal is preserved. Continuation reads only the retained
prefix, not a complete full log. Artifact creation/privacy failures still refuse
to start the command. Descendant preservation on natural leader completion is
not guaranteed; running-process #1346 and the remainder of clud #2298 track it.

## Storage

`<repo>/.clud/test-runtime/runs.jsonl`, one JSON object per line, gitignored and
per-checkout. Fields are short because they repeat every row: `v` (schema
version), `b` (bucket), `t` (target, omitted for a whole-bucket run), `ms`,
`cpu`, `at`, `rc`, `os`.

**#407 specifies SQLite via `rusqlite`; this uses JSONL.** Its acceptance
criteria permit a deviation with rationale: `rusqlite` is not a dependency here
and was removed deliberately (see `Cargo.toml`'s note on the swap to `redb` "to
cut cold-build time and drop the C-toolchain pressure on CI"). The accepted
design works through redb too and rejects it — redb takes an exclusive
per-process file lock, the problem [DD-006](../../../../docs/DESIGN_DECISIONS.md)
needed a daemon and an advisory lockfile to work around.

## Two invariants worth not breaking

1. **Append is one `write_all` including the newline.** `writeln!` issues the
   content and the newline as separate writes, which lets a concurrent appender
   land between them and merges two records into one unparseable line. Measured:
   43 of 100 records survived with four writers before this was fixed.
   `concurrent_writers_do_not_corrupt_the_store` pins it.
2. **Compaction takes the lock; appends do not.** Compaction is a
   read-modify-write and races appends without one; the lock is only taken once
   a bucket is actually over `MAX_ROWS_PER_BUCKET`, so an ordinary write pays a
   read and nothing else.

## Not in v1

Bucket heuristics, the CPU-normalization formula, the run-all-vs-targeted
threshold, `bash test` integration, and CI bootstrap are #405 Q2/Q3/Q6/Q8/Q9 and
are explicitly deferred. v1 ships the storage and the dumbest useful read/write
surface; the *agent* makes the decision.

## Used by

`main.rs` dispatches `Command::Test`. Registering the subcommand touched four
places, not the usual three — see CLAUDE.md's cross-cutting registry list for
`SEPARATOR_OWNING_SUBCOMMANDS`, which a `trailing_var_arg` subcommand must join
or it silently receives no command at all.

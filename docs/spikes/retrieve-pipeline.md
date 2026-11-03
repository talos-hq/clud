# Retrieval pipeline: bounded evidence spike

2026-11-03. Branch `spike/retrieve-pipeline`, based on the existing
`spike/multicmd-byte-budget` snapshot. Local subscription-only experiment:
one Codex session, deterministic Python stdlib + rg, no APIs, embeddings,
LLM reranking, or subagents. No push, merge, tag, release, hook installation,
or changes to the other checkout.

## Hypothesis and decision

A single deterministic retrieval call can replace the rg/read decision loop
with a better evidence pack inside 4,096 bytes. **Not supported by this run.**
A returned 14,028 bytes in 20 calls; E returned 31,558 bytes in 10 calls.
Exact source-anchor recall@5 was A 10/10, E 3/10. E needs seven repairs under
our explicit oracle-assisted repair rule. Bounding output works; selecting the
right evidence remains the bottleneck.

Decision: **revert adoption**, retain the small, self-contained unbundled
`code/retrieve.py` solely on this spike branch for reproduction. No production
code was edited, so there is no runtime patch to undo. The pre-existing spike
code inherited from the parent branch is unchanged. Main and the other branch
are unchanged. Keep `code/retrieve_bench.json` and this report.

## Method and reproducibility

The JSON freezes ten real repo questions, gold source path/line/anchor and
SHA-256, actual A commands, E command, returned E evidence, byte counts,
latencies and repair commands. Gold anchors were selected by a separate source
inspection before the benchmark, not derived from E's ranked results. Source
commit is recorded in the JSON. Verify each anchor and file hash against that
snapshot. The dispatch/matcher questions intentionally share a registration
site but test different facts. This small, purposive set is not representative
of all repo work; six questions concern hooks/hunts.

A is a **researcher-scripted, filename-aware baseline**, not a trace of an
unaided model. Each task runs filename-scoped `rg -n`, keeps its first five
matching locations, merges overlapping windows of five preceding and 25
following lines, then batches them into one `sed -n` call. The actual two
commands are stored per task. This is today's rg + source-read workflow;
`read_file` is represented by sed, not an available tool invocation here.
The baseline's manually supplied filenames and search patterns are privileged
input: E receives only the natural-language query, including identifiers where
present. Thus this experiment compares E with an informed workflow, not equal
information discovery. It cannot establish that all deterministic retrieval is
worse. A follow-up must remove this asymmetry.

E runs `python code/retrieve.py '<query>'` once with the default total 4,096-byte
cap. Internal passes do not count as model-facing calls. Both arms capture
stdout and stderr as bytes before any harness wrapper; metadata and continuation
footers count. POSIX `os.posix_spawnp` redirects both streams into a temporary
file, avoiding pipe deadlocks and third-party process dependencies. Timings use
`perf_counter`; A and E run serially per task. This is one run, uncontrolled
warm filesystem, with concurrent local validation: latencies are descriptive,
not statistically stable estimates. A separate exploratory decode probe and
one preliminary batch preceded the reported final run. The final batch adds
an explicit word-boundary exact-token search pass; it changes hunt's returned
bytes but no recall outcome. No ranking weights were fitted to gold results.

Recall@5 means the exact gold path and line are contained in one of the first
five **actually emitted** ranges; it is binary per task, averaged over tasks.
It does not measure comprehension or whether a merely relevant document
answers the question. On an E miss we executed one gold-aware sed repair and
recorded its actual bytes/time separately. This is an optimistic repair cost,
not an observed model trajectory or a claim that one unaided follow-up suffices.
No continuation traversal was used to manufacture first-call recall.

## Prototype

The standalone POSIX script uses stdlib and `rg` only; it is not registered,
installed, or added to the bundled-tool registry. Scope is inferred softly
from query identifiers and filename matches. Concurrent passes search exact
word-boundary tokens, snake/camel/path identifier variants, filename-prioritized
files and the literal phrase. Filename candidates are capped at 80; each rg
pass caps matches at 40 per file. Candidate order and all ties are deterministic.
Ranking combines line term coverage, path relevance, enclosing heading/symbol
coverage, definition bonus, test-path penalty and match-to-boundary proximity.
Overlapping/adjacent ranges are merged up to a 45-line span, or redundant wider
windows are dropped. Cheap lexical function/heading expansion falls back to
seven-line context for large sections. Every emitted item includes path, range,
symbol/heading and score reasons.

This is **lexical** expansion, not an AST guarantee of complete functions:
Rust methods, multiline signatures, braces in strings and Python comments
confused with Markdown headings are limitations. The search universe includes
Rust, Python and Markdown under crates/src/docs/ci; other languages and paths
can be missed. Internal search output and ranking work are not byte-bounded;
only model-facing output is. Recomputing lexical definitions per hit is also
expensive and contributes to E latency.

Rendering reserves 160 bytes for truncation/continuation status and admits
only complete evidence items, at most five. An oversized next item stops the
pack and explicitly requests a larger budget; it does not silently skip that
item. Handles are content-fingerprint + candidate index; repeat the same query
with `--continue HANDLE`. Corpus/ranking changes reject stale handles. A handle
is a cursor, not a saved artifact. Headers can name an enclosing symbol even
when the fallback window only shows part of it. All ten measured outputs were
below the cap; smoke checks also verified UTF-8 accounting, advancing cursors
and stale-handle rejection.

## Per-task results

A / E pairs. Calls and bytes are initial retrieval; repairs are separate.
Gold facts and full commands live in the benchmark JSON.

| Task | Calls A / E | Bytes A / E | Seconds A / E | Recall A / E | Repairs A / E |
| --- | ---: | ---: | ---: | ---: | ---: |
| dispatch | 2 / 1 | 1,896 / 3,946 | 0.0068 / 2.326 | 1 / 0 | 0 / 1 |
| decode | 2 / 1 | 2,029 / 3,632 | 0.0076 / 3.386 | 1 / 0 | 0 / 1 |
| notice | 2 / 1 | 1,050 / 2,445 | 0.0047 / 1.809 | 1 / 0 | 0 / 1 |
| matcher | 2 / 1 | 1,896 / 3,421 | 0.0344 / 5.503 | 1 / 0 | 0 / 1 |
| hunt | 2 / 1 | 1,288 / 2,018 | 0.0049 / 4.270 | 1 / 0 | 0 / 1 |
| home | 2 / 1 | 865 / 2,765 | 0.0051 / 2.935 | 1 / 1 | 0 / 0 |
| purge | 2 / 1 | 1,292 / 3,638 | 0.0060 / 4.895 | 1 / 1 | 0 / 0 |
| flags | 2 / 1 | 1,014 / 3,516 | 0.0056 / 3.473 | 1 / 0 | 0 / 1 |
| skills | 2 / 1 | 1,337 / 2,668 | 0.0052 / 4.954 | 1 / 0 | 0 / 1 |
| hooks | 2 / 1 | 1,361 / 3,509 | 0.0048 / 6.268 | 1 / 1 | 0 / 0 |

Aggregate initial retrieval: **14,028 / 31,558 bytes**, **20 / 10 calls**,
**0.0849 / 39.8189 seconds**, **100% / 30% recall@5**.
E halves initial calls but increases bytes by 125.0%; its largest output is
3,946 bytes. Seven measured oracle repairs add 8,039 bytes and 0.0195 seconds.
Including these repairs E totals **17 calls, 39,597 bytes, 39.8384 seconds**
with gold coverage repaired to 10/10. That does not change initial recall@5.
These sums exclude authoring, independent gold verification, exploratory runs
and validation; they are retrieval outputs, not subscription billing or total
conversation-context savings.

## Failure analysis

- `dispatch`: hook-dispatch documentation outranks the actual JSON registration.
- `decode`: generic stdout headings and tool-output helpers swamp the rare
  `tool_response` relationship; individual term matches do not bind a field chain.
- `notice`: project trust helpers outrank the notice merger; verb stemming and
  the relationship between project hook and notice are absent.
- `matcher`: splitting PostToolUse into common post/tool/use terms promotes test
  functions and telemetry over the Bash registration. Definition bonuses amplify it.
- `hunt`: descriptive test names outrank `hunt_ranges`; the weak test penalty
  cannot counter term/heading coverage. Budget consumption prevents further hits.
- `flags`: architecture prose and near-miss tests outrank `split_known_unknown`.
- `skills`: explicit-invocation documentation and installer tests displace the
  implementation. Lexical '#' recognition also treats Python comments as headings.

Home resolution, retired-tool purge and tool-name matching succeed. These have
strong identifier/path alignment. Good ranking scores do not imply useful
implementation evidence; the cap makes a ranking mistake consequential because
lower candidates never reach the context window. Search omission, ranking and
budget loss are not separately quantified by this first experiment.

## One next experiment

Run a **matched-scope evidence-pack ablation**: give A and E exactly the same
filename hint and query, pre-register ten new held-out questions, and require
implementation-anchor recall noninferior to A before comparing bytes/calls.
Keep the 4 KiB cap and the ranking weights frozen. This isolates the value of
in-call packing from the privileged scoping advantage in this benchmark;
do not add embeddings, APIs or another model. Report first-call recall and
actual continuation costs separately.

## Experiment ledger

| Date | Experiment | Decision | Evidence | Next |
| --- | --- | --- | --- | --- |
| 2026-11-03 | One-call deterministic 4 KiB retrieval | Revert adoption; keep unbundled spike prototype + benchmark/report | A/E: 14,028/31,558 B; 20/10 calls; 10/3 gold hits | Matched-scope held-out ablation |

## Validation after runtime-change cleanup

No production changes were introduced or reverted in this experiment. Compared
with the starting commit, only the prototype, benchmark JSON and this document
are added. The working tree contains no new runtime patch to test.

- `ruff check code/retrieve.py`: all checks passed; Ruff formatting applied.
- UTF-8 hard-budget, cursor advancement and stale-handle smoke checks: PASS.
- `SOLDR_PORCELAIN=1 soldr cargo test -q -p clud --lib block_bad_cmd_hunt -- --test-threads=1`:
  `{"duration_ms":70261,"exit_code":0,"status":"ok"}` (porcelain excerpt).
- `SOLDR_PORCELAIN=1 soldr cargo test -q -p clud --lib` failed, exit 101:
  `test result: FAILED. 5175 passed; 6 failed; 12 ignored; 0 measured; 0 filtered out; finished in 37.90s`.
  Four `daemon::http` tier1 tests require a built mock-agent; two process_scan
  assertions expect an undeclared self but this session has a daemon marker.
  This run does **not** establish a green repository test suite.
  Full log: `/home/niteris/.soldr/logs/porcelain/20261103T093959Z-home-niteris-dev-clud-wt-hunt.log`.
- Documented quick lane `clud tool run diagnostics/test_report.py -- bash test --host`:
  exit 1, `test result: FAILED. 5163 passed; 2 failed; 12 ignored; 0 measured; 16 filtered out; finished in 38.78s`.
  Only the same two process_scan tests failed; Tier 1 mock-agent tests were
  excluded correctly. Python tests were not reached because the Rust lane failed.
  `RUNNING_PROCESS_IS_DAEMON` is present in this session; the assertions explicitly
  assume an undeclared self. No test or runtime code was changed to mask this.
  Full artifact: `/home/niteris/.clud/tmp/tool-output/local-test-private/local-test-0tfGOd.log`.
- `bash lint --host-only`: exit 0; formatting, Ruff, repository guards, host Clippy and Dylint completed. Existing dependency future-incompatibility warning for nix 0.28.0.
- `git diff --cached --check`: passed before commit.

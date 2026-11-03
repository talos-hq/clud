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


## Round 3 — matched-scope ablation

2026-11-03. Starting snapshot `6b9847a4` on `spike/retrieve-pipeline`.
Subscription-only, one Codex model/session, deterministic local tools, no API,
embeddings, LLM reranking, or agents. Retrieval only reads repository content;
only the unbundled spike prototype, benchmark artifacts and this report change.

Hypothesis: path/layout scoping plus definition-first ranking and a 1,536-byte
output cap will achieve at least 9/10 exact-anchor recall@5 while reducing calls.
At <=6/10 the pipeline would be rejected outright. **Observed 7/10: target not
met, between the stated success and outright-rejection thresholds. Do not ship.**

### Held-out set and method

`code/retrieve_bench_heldout.json` freezes ten NEW questions and twelve source
anchors (two two-anchor traces), with exact lines, text and
SHA-256 hashes. None reuses a Round 1 ground truth. Anchors were inspected and
saved before ranking edits. Categories include two documentation sections,
`completion-trace` across loop_check/loop_spec, and `escape-trace` across
bracketed_paste/escape_gate. A trace passes only if BOTH gold anchors appear in
the first five emitted ranges. The config-conflict anchor is the actual
validation branch, rather than the parse signature; marker-priority requires
the DONE branch, rather than a read_markers_at declaration/call.

Bare lexical first-five matching misses those last two exact anchors (and the
escape trace); source windows recover the config and marker answers. These are
lexical/context traps, not proof that a skilled rg user cannot answer them.
`origin` is a straightforward default-variant lookup, not a lexical failure.

Reproduce: `python code/retrieve_bench_heldout.py`. It verifies gold hashes and
records commands, outputs, ranges, bytes, latency and repairs in the JSON.
Both arms get the identical explicit file list. A uses the pre-registered
lexical pattern and rg --sort path, then one batched source read with -5/+25
windows around its first five hits. E2 receives the natural query plus repeated
`--scope` arguments. A remains researcher-scripted, not an observed unaided
model; it also receives a hand-selected search expression. This controls file
scope, not query formulation. Total rg stdout counts toward A bytes, including
hits after the five chosen for reading; sed represents read_file. Batched
shell reads count as one model-facing call. Internal E2 rg passes are not
model-facing calls. Follow-ups are separate, executed oracle-assisted source
reads, one batched repair per failed task; these are optimistic costs.

An exploratory A implementation ORed all query terms (135,593 bytes, 1/10),
which is a weak baseline. The reported A uses the patterns already frozen in
the task specification. A preliminary patterned run lacked stable multi-file
ordering; the final run adds --sort path. No E2 ranking weights or gold anchors
were changed in response to those scores. The final table is one warm run, not
a latency distribution; both arms ran while local validation was active.

Explicit matched hints are an optimistic scoping ablation, not evidence of
end-to-end automatic scope discovery. Automatic inference is only smoke-tested.

### Prototype changes

Reuse the existing search/rank/render implementation: explicit --scope restricts
all passes; without it, exact query-to-file-stem matches narrow the search roots,
with broad fallback. Cache lexical definitions once per file. Definition matches
rank above doc comments, then calls, then incidental matches; lexical term,
heading and path weights break ties. Python comments no longer count as Markdown
headings. Match-centered small ranges replace potentially huge function windows.

Default output cap is 1,536 UTF-8 bytes, including provenance and footer. The
renderer admits at most five ranked items, shortening source prefixes to fit,
and reports omitted candidate counts plus omitted_lines when shortening an
item. Every emitted item retains path/range, enclosing symbol, scoring reasons
and line numbers. Continuations keep the original fingerprint/cursor policy.
A shortened item advances the cursor: its omitted tail requires an explicit
source read, not merely the next cursor. Candidate search work itself remains
unbounded by the model-facing cap. These are lexical boundaries, not AST spans.

### Per-task results

Initial retrieval, A / E2. Recall is binary all-anchor coverage within five
ranges; follow-ups are separately measured repairs.

| Task | Bytes A / E2 | Calls A / E2 | Recall A / E2 | Follow-ups A / E2 | Seconds A / E2 |
| --- | ---: | ---: | ---: | ---: | ---: |
| classify | 4,683 / 1,372 | 2 / 1 | 1 / 1 | 0 / 0 | 0.0257 / 0.1609 |
| short-form | 1,996 / 1,351 | 2 / 1 | 1 / 1 | 0 / 0 | 0.0191 / 0.1581 |
| marker-priority | 2,501 / 1,336 | 2 / 1 | 1 / 1 | 0 / 0 | 0.0208 / 0.1989 |
| config-conflict | 3,317 / 1,365 | 2 / 1 | 1 / 0 | 0 / 1 | 0.0181 / 0.1951 |
| config-overlay | 1,261 / 1,278 | 2 / 1 | 1 / 1 | 0 / 0 | 0.0158 / 0.1524 |
| origin | 2,288 / 1,283 | 2 / 1 | 1 / 1 | 0 / 0 | 0.0151 / 0.1704 |
| completion-trace | 4,473 / 1,434 | 2 / 1 | 1 / 0 | 0 / 1 | 0.0373 / 0.2073 |
| escape-trace | 6,280 / 1,422 | 2 / 1 | 0 / 0 | 1 / 1 | 0.0193 / 0.2040 |
| ci-tiers | 1,986 / 1,339 | 2 / 1 | 1 / 1 | 0 / 0 | 0.0156 / 0.1503 |
| ci-queue | 2,050 / 1,378 | 2 / 1 | 1 / 1 | 0 / 0 | 0.0127 / 0.1315 |

Aggregate initial retrieval: **30,835 / 13,558 bytes**, **20 / 10 calls**,
**9/10 / 7/10 recall@5**, **1 / 3 follow-ups**, **0.1997 / 1.7291 seconds**.
E2 reduces bytes by 56.0% and calls by 50%, but loses two task hits against A
and is 8.7x slower. Every E2 task is below 1,536 bytes (largest 1,434).
The smaller cap is enforced; this does not establish optimal evidence packing.
Oracle repairs add A/E2 555/1,560 bytes and 0.0152/0.0430 seconds. Including
repairs, totals are 31,390/15,118 bytes and 21/13 calls (coverage repaired to
10/10 for both). That does not change first-call recall.

### Failure analysis and decision

- `config-conflict`: parse/type declarations and module comments outrank the
  actual done/no_done condition at line 86. Its candidate exists, but is omitted.
  The parse signature alone does not answer the validation question.
- `completion-trace`: the callee definition is returned, but unrelated marker
  definitions displace the caller edge at loop_check:34. Returning the caller's
  signature at line 24 does not count as returning its call at line 34.
- `escape-trace`: the gate struct, normalizer struct, push signature and a test
  definition outrank the self.gate.push call at bracketed_paste:129. A path-only
  test penalty fails for inline Rust tests. A also misses this full trace.

All three E2 gold locations exist in searched files and in the candidate pool;
this is ranking/packing loss, not missing files. Absolute definition precedence
helps symbol lookups but harms conditions and relationships. Tiny windows and
prefix shortening amplify that effect. Seven successes do not justify adoption,
and this small, deliberately scoped sample cannot establish general recall.

**Decision: retain research artifacts only; revert adoption / do not ship.**
There were no production-code edits to revert. Keep this local spike branch;
no push, merge, main update, tag, new workflow or registry integration.

**ONE next experiment:** freeze the same scopes, candidate pool and 1,536-byte
cap, then ablate hit-centered evidence selection against absolute
definition-first selection: prioritize exact query-identifier occurrences and
retain the matching condition/call line instead of spending the pack on generic
declarations. Measure all-anchor trace recall and repair bytes on another frozen
held-out set. This isolates evidence selection without adding retrieval systems.

### Validation after changes

- `ruff check code/retrieve.py code/retrieve_bench_heldout.py`: passed.
- Gold hashes checked by the runner; all outputs capped. Additional smoke checks:
  40 budgets across ten tasks (256/512/1024/1536 bytes), ten continuations,
  stale-handle rejection, Unicode oversized lines, and exact-stem scope inference:
  PASS. No cap or continuation tests change runtime code.
- `clud tool run diagnostics/test_report.py -- bash test --host`: exit 1.
  Failure-focused tail (full artifact retained at
  `/home/niteris/.clud/tmp/tool-output/local-test-private/local-test-v4LHyx.log`):

```text
failures:
    daemon::server::tests::shutdown_and_signal_flag_finish_within_the_restart_timeout
    process_scan::tests::a_repeated_ask_about_known_identities_is_served_from_cache
    process_scan::tests::scan_is_callable_and_excludes_an_undeclared_self
    voice::mode::tests::voice_config_can_enable_from_model_path_without_flag

test result: FAILED. 5161 passed; 4 failed; 12 ignored; 0 measured; 16 filtered out; finished in 55.70s
error: test failed, to rerun pass `-p clud --lib`
```

The daemon test logs a GC database lock and startup timeout; process_scan asserts
that this process is not a declared daemon (the same assumptions failed in the
previous round); voice asserts config.enabled. No production changes were made,
and these diagnostics do not prove the cause of the voice failure. Python tests
were not reached. The repository quick lane is **not green**. No exclusions or
runtime patches were added to hide failures.

- The lint report wrapper could not produce a complete safe report. The bounded
  fallback reran `bash lint --host-only`: **exit 0**. Ruff and repository guards
  passed; soldr reused passing fmt, Clippy and Dylint verdicts. Full log:
  `/tmp/retrieve-round3-lint.log`. Final tail:

```text
info: component llvm-tools is up to date
soldr: dylint ready on channel nightly-2026-08-15-x86_64-unknown-linux-gnu
soldr: dylint skipped: inputs match a passing run
```

- Miss diagnostics: config condition rank 11; completion caller rank 26 (callee
  rank 1); escape caller rank 21 (push rank 3). These ranks were inspected after
  freezing results and did not drive another weight adjustment.
- `git diff --check`: passed. Only spike artifacts changed from the starting
  snapshot; no production code needed reverting.

## Round 4 — recall floor

2026-11-03. Subscription-only Codex via clud; no paid API, agents, embeddings or
LLM reranking. Deterministic, read-only retrieval. Only the unbundled spike
prototype, benchmark runner/results and this report change.

Hypothesis: preserve EVERY exact lexical query-anchor hit independently of
ranking/deduplication, achieving recall@5 >= A on BOTH frozen sets, with bytes
<1.6x A and calls <=0.6x A. **Failed; stop. Do not ship.**

### Implementation and measurement

`code/retrieve.py` now gathers an uncapped, case-insensitive whole-token literal
pass separately from the old capped candidate passes. Tokens are the existing
`terms(query)` normalization (original tokens plus underscore/camel/path splits,
minus existing stopwords); substrings inside longer identifiers do not qualify.
The floor contains every matching source line, once per path/line, with all
matched literal tokens: `! path:line anchors=...`. Stable path/line order has no
relevance ranking. Ranking/deduplication still controls only optional context.
No gold terms, task-specific weights or extra filename hints were added.

The 1,536-byte cap includes all emitted material. The floor comes first; context
uses only the remaining budget. If the floor alone exceeds the cap, all anchors
are emitted and there are no expansions or footer. This is the requested
explicit exception to the budget, not a claim that every pack is bounded.
Compact floor entries omit enclosing-symbol annotations to minimize overhead.

Reproduce: `python code/retrieve_bench_round4.py`. Both existing JSON files are
read as frozen inputs and retained unchanged; full commands, evidence, emitted
locations, gold hashes, first-five recall, all-emitted coverage, measured timings
and oracle-assisted repair costs are in `code/retrieve_bench_round4.json`.
All 22 gold anchors/hashes were verified before executing their task.
Original A reruns its frozen two commands; held-out A reruns Round 3's sorted rg
and first-five -5/+25 source windows. E3 uses the original natural query and scope
policy: automatic inference for the original set, identical explicit scopes for
the held-out set. Thus the original set retains its known scope asymmetry; this
is not a new matched-scope ablation for that set. No scope changes were fitted.

Calls count model-facing retrieval calls (A=2, E3=1); local internal searches
are not model-facing calls. Bytes include all raw stdout/stderr. Seconds are one
warm serial paired run, with local validation active, not a latency distribution.
An initial measured run preceded a renderer-only correction: overflow output is
now floor-only, and a floor below budget cannot exceed it by adding a footer.
No ranking, query, scope or gold adjustment followed either run.

Recall@5 retains the frozen definition: all gold path/lines must occur within the
first FIVE actually emitted locations/ranges. Each compact anchor is one location;
an exhaustive appendix is not counted as a single range or as a first-five hit.
All-emitted coverage is reported separately, never substituted for recall@5.
Repairs remain one executed gold-aware batched source read per miss, an optimistic
cost rather than an observed unaided model trajectory.

### Held-out per-task results

Initial retrieval, A / E3; follow-ups and their bytes are separate.

| Task | Bytes A / E3 | Calls A / E3 | Recall@5 A / E3 | Follow-ups A / E3 | Seconds A / E3 |
| --- | ---: | ---: | ---: | ---: | ---: |
| classify | 4,683 / 3,537 | 2 / 1 | 1 / 0 | 0 / 1 | 0.0139 / 0.0844 |
| short-form | 1,996 / 3,147 | 2 / 1 | 1 / 0 | 0 / 1 | 0.0111 / 0.0954 |
| marker-priority | 2,501 / 5,449 | 2 / 1 | 1 / 0 | 0 / 1 | 0.0107 / 0.0914 |
| config-conflict | 3,317 / 1,877 | 2 / 1 | 1 / 0 | 0 / 1 | 0.0085 / 0.0974 |
| config-overlay | 1,261 / 1,490 | 2 / 1 | 1 / 0 | 0 / 1 | 0.0085 / 0.0882 |
| origin | 2,288 / 1,408 | 2 / 1 | 1 / 0 | 0 / 1 | 0.0093 / 0.0844 |
| completion-trace | 4,473 / 5,385 | 2 / 1 | 1 / 0 | 0 / 1 | 0.0140 / 0.1082 |
| escape-trace | 6,280 / 4,114 | 2 / 1 | 0 / 0 | 1 / 1 | 0.0110 / 0.0986 |
| ci-tiers | 1,986 / 5,935 | 2 / 1 | 1 / 0 | 0 / 1 | 0.0091 / 0.0693 |
| ci-queue | 2,050 / 1,543 | 2 / 1 | 1 / 0 | 0 / 1 | 0.0075 / 0.0668 |

### Aggregates for both sets

| Set | Bytes A / E3 | Calls A / E3 | Recall@5 A / E3 | Follow-ups A / E3 | Seconds A / E3 |
| --- | ---: | ---: | ---: | ---: | ---: |
| original | 14,028 / 812,965 | 20 / 10 | 10/10 / 0/10 | 0 / 10 | 0.1220 / 5.2603 |
| heldout | 30,835 / 33,885 | 20 / 10 | 9/10 / 0/10 | 1 / 10 | 0.1037 / 0.8843 |
| combined | 44,863 / 846,850 | 40 / 20 | 19/20 / 0/20 | 1 / 20 | 0.2257 / 6.1446 |

E3 bytes are 57.95x A on original, 1.10x on held-out, and 18.88x combined.
Calls are 0.50x A in both sets, meeting the initial-call threshold. Bytes meet
1.6x only on held-out, and recall fails on BOTH sets. Six original and eight
held-out packs exceed 1,536 bytes because the floor alone exceeds the cap.
Oracle repairs add original A/E3 0/2,991 bytes and 0/0.0637 seconds;
held-out 555/3,341 bytes and 0.0084/0.0626 seconds. Including repairs, combined
A/E3 costs are 45,418/853,182 bytes and 41/40 calls. These repairs do not improve
first-call recall@5 and erase most of the call advantage.

### Misses, reasons and decision

All ten held-out tasks miss recall@5, although all twelve held-out gold anchors
are emitted somewhere. Their 1-based emitted positions are:

| Task | Gold positions in E3 output | Reason for recall@5 miss |
| --- | --- | --- |
| classify | 15 | Earlier lexical occurrences fill the first five. |
| short-form | 20 | Earlier lexical occurrences fill the first five. |
| marker-priority | 31 | Earlier lexical occurrences fill the first five. |
| config-conflict | 21 | Earlier lexical occurrences fill the first five. |
| config-overlay | 14 | Earlier lexical occurrences fill the first five. |
| origin | 8 | Earlier lexical occurrences fill the first five. |
| completion-trace | 11, 79 | Earlier lexical occurrences fill the first five. |
| escape-trace | 26, 43 | Earlier lexical occurrences fill the first five. |
| ci-tiers | 14 | Earlier lexical occurrences fill the first five. |
| ci-queue | 17 | Earlier lexical occurrences fill the first five. |

In particular, config-conflict's condition survives at position 21;
completion-trace's caller/callee survive at 11/79; escape-trace's caller/callee
survive at 26/43. The suspected ranking loss is real for these locations, but
merely retaining them does not establish useful first-five recall. Common query
words generate many exact matches, especially in docs and tests.

All ten original tasks also miss first-five recall. `decode`, `hunt` and `home`
have all-emitted coverage, at positions 842, 563 and 7. `dispatch`, `notice`,
`purge`, `flags` and `hooks` lack their gold files because unchanged exact-stem
scope inference selects hook/dispatch, hook, tools, backend and hook files,
respectively. `matcher` finds PostToolUse but its gold matcher line has no exact
non-stopword query token; context is dropped behind a 424,825-byte floor.
`skills` gold is a longer underscore identifier, not a whole-token match for
explicit/invocation/policies; its expansion is likewise dropped. These are
scope or lexical-definition limitations, not dropped exact anchors. Retention
is guaranteed only inside the selected scopes and existing source-type globs.
A misses only escape-trace, as in Round 3.

**Decision: keep research artifacts only; revert adoption / do not ship.**
The recall floor remains solely in the unbundled spike prototype. No production
code changed, so there was no production patch to revert. Remain on
`spike/retrieve-pipeline`; no merge, tag, push to main, workflow or integration.
No further retrieval experiment was run after rejecting this hypothesis.

**ONE next highest-value experiment:** on the frozen matched-scope held-out set,
order compact exact anchors by deterministic query-token specificity (inverse
file frequency), retaining the exhaustive floor after the first-five preview.
Keep gold, scopes and budget policy fixed. This tests whether relevance ordering
can restore first-five trace recall without losing the retention guarantee;
it does not assume that an exhaustive floor itself supplies first-five recall.

### Validation

- Independent full-file Python regex enumeration matched all emitted floor
  locations on all 20 tasks, with no per-file hit cap. Every budget overflow
  consisted solely of a floor already exceeding the budget. Empty-term query
  smoke check passed. All frozen gold hashes/anchors verified.
- `ruff check code/retrieve.py code/retrieve_bench_round4.py`: passed;
  `ruff format` applied. `git diff --check`: passed.
- `bash lint --host-only`: exit 1 at the interpreter-name guard, after Ruff and
  preceding repository guards passed. The unchanged Round 3 held-out JSON
  contains ten captured resolved interpreter paths named `python3.13`.
  Full log: `/tmp/retrieve-round4-lint.log`. Historical command evidence was
  not rewritten to manufacture a green gate. Full repo lint is not green.
- `clud tool run diagnostics/test_report.py -- bash test --host`: exit 1.
  The documented lane invokes Rust through soldr. Failure tail:

```text
failures:
    process_scan::tests::a_repeated_ask_about_known_identities_is_served_from_cache
    process_scan::tests::scan_is_callable_and_excludes_an_undeclared_self

test result: FAILED. 5163 passed; 2 failed; 12 ignored; 0 measured; 16 filtered out; finished in 27.01s
error: test failed, to rerun pass `-p clud --lib`
```

Full artifact:
`/home/niteris/.clud/tmp/tool-output/local-test-private/local-test-jOKgZY.log`.
These two process_scan assumptions also failed in Round 3. No production changes
or test exclusions were used to hide them; Python tests were not reached.

Final-state rerun: the report wrapper returned `unable to produce a complete safe
report`; the bounded fallback `bash test --host > /tmp/retrieve-round4-test-final.log
2>&1` completed with exit 1. Final tail (same failures, no exclusions):

```text
failures:
    process_scan::tests::a_repeated_ask_about_known_identities_is_served_from_cache
    process_scan::tests::scan_is_callable_and_excludes_an_undeclared_self

test result: FAILED. 5163 passed; 2 failed; 12 ignored; 0 measured; 16 filtered out; finished in 23.02s
error: test failed, to rerun pass `-p clud --lib`
```

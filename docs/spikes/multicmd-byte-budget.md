# Multi-command byte budget: throwaway spike

2026-11-03. Branch `spike/multicmd-byte-budget`, from `feat/read-hunt-slice`
(`5afd94038b813802116b89133ade4f739b4e2d98`; parent PR talos-hq/clud#2593).
Scope: one local experiment; no merge, release, or production recommendation by default.

## Dispatch verdict

**No new dispatch point is needed for Claude Bash.**
`clud_hooks_compile.rs:115-118` already registers `PostToolUse` for Bash;
`block_bad_cmd.rs:442-479` decodes and retains the raw JSON payload and observes
finished calls. `block_bad_cmd_grind_gate_evidence.rs:90-125` already reads
`tool_response.stdout`. The existing `hookSpecificOutput.additionalContext`
channel can append one context line after execution. It cannot edit the original
shell result in place or recover bytes already exposed. The prototype uses that
channel at the final allowed-output path and merges the existing project-hook
notice; it does not add a registration, event, or denial contract.

Limitations: this proves a local Claude Bash path, not every backend. The existing
post matcher does not promise Codex `exec_command` output. Missing/unknown response
shapes, malformed or oversized hook JSON, unsupported commands and bypasses fail
open. Counted bytes are UTF-8 stdout + stderr present in the hook payload, not JSON
serialization or a tool wrapper, nor output omitted/truncated by the harness.

## Prototype

`block_bad_cmd_hunt_spike.rs` extends detection with a deliberately literal grammar:
`awk 'NR >= A && NR <= B' FILE`, `cat -n FILE`, literal-pattern
`grep -A20/-B20` (or larger attached context), and a semicolon-separated
`head -n N FILE; tail -n N FILE` pair. Matching grep windows become individual
ranges. It reuses `Hunt`, its signature, session state, one-hour inactivity reset,
8 KiB source-file floor, 80 requested-line floor, refusal-once/retry behavior,
and both escape hatches. A single new idiom can qualify; one ordinary sed range
still cannot. Unknown flags, pipelines, redirects, expansions and grammar variants
fail open. Whole-file/tail/grep classification reads the file locally: this is
extra hook I/O, not free inference. Generic awk actions and arbitrary grep regex
semantics are intentionally unsupported.

One source-contract correction: `decide()` increments the hunt count on every
qualifying occurrence, including repeats; the **refusal** is signature keyed.
It does not count only distinct hunts. This experiment preserves that behavior.

The post-output nudge triggers strictly above 4,096 bytes for supported reads and
appends exactly one line, e.g.:

```text
[clud byte-budget] 20743 bytes returned; next narrower read: `clud tool run code/slice.py crates/clud-bin/src/block_bad_cmd_hunt.rs --lines 1-40 --max-bytes 4096`
```

It never blocks. It advises the first 40 requested lines, not a guaranteed unseen
continuation; without knowing the question, selecting the next useful lines is
impossible. Unsupported commands get no speculative replacement. This is a
limited proof, not a universal shell-output budget. Existing overrides suppress
it. The original output remains intact and the notice itself adds bytes.

## Measurements

Local real file `F=crates/clud-bin/src/block_bad_cmd_hunt.rs`: 478 lines, SHA-256
`7f5dd5dc0714c86814d5d3b3ec15455ca3faf8dabeca2e6b63b13704af5fbd4c`
(after prototype wiring). Commands were executed with Python `subprocess.run`,
stdout/stderr captured as bytes and counted with `len(stdout)+len(stderr)`;
no source output was forwarded into the conversation. Each row is one shell
call before and one after. No latency, model-comprehension or behavioral experiment
was performed. Five examples are not a fleet prevalence estimate.

`S` below means `clud tool run code/slice.py "$F"`. For measurement the checked-out
implementation was invoked as `python crates/clud-bin/assets/tools/code/slice.py "$F"`
to avoid measuring an older installed bundle. Headers, line numbers and truncation
footers are included. Original commands all exited 0. Slice exit 3 means partial
output, not a failed subprocess measurement.

| Idiom / command (`F` as above) | Replacement (`S` as above) | Today bytes | Replacement bytes | Reduction | Coverage |
| --- | --- | ---: | ---: | ---: | --- |
| `sed -n '40,180p' "$F"; sed -n '120,260p' "$F"` | `S --lines 40-180 --lines 120-260` | 9,736 | 8,277 | 15.0% | Overlap merged; cap truncates (exit 3) |
| `awk 'NR >= 40 && NR <= 260' "$F"` | `S --lines 40-260` | 7,893 | 8,277 | -4.9% | Same requested range; cap truncates (exit 3) |
| `cat -n "$F"` | `S --lines 1-40 --max-bytes 4096` | 20,743 | 2,309 | 88.9% | Deliberately only first 40 lines (exit 0) |
| `grep -A20 fn "$F"` | `S --grep fn -C 2 --max-hits 5 --max-bytes 4096` | 11,630 | 1,075 | 90.8% | Less context, first five hits (exit 3) |
| `head -n 100 "$F"; tail -n 100 "$F"` | `S --lines 1-100 --lines 379-478` | 8,058 | 8,200 | -1.8% | Same requested ranges; cap truncates (exit 3) |

Total: 58,060 → 28,138 bytes (51.5% less), **not equivalent information**.
At the supplied mean 71 re-sends this difference models 2,124,462 bytes less
exposure, provided no extra turns or continuation reads are required. This is
arithmetic on the sample, not an observed end-to-end context reduction. The awk
and head/tail counterexamples disprove “slice substitution always saves bytes.”
Even the nominal 8 KiB cap is not a hard total-output bound: footer/row accounting
can exceed it (8,277 bytes here). A 4 KiB setting is likewise an approximate budget.

## Optimum and recommendation

There is no measured universal optimum in these five reads. For already-needed
independent ranges, batch them in one bounded call: splitting the same content
adds a turn and another re-send of the existing window. If a wider read adds
`D` unnecessary bytes and they survive `R` later sends, its extra cost is about
`R*D`; an extra narrowing turn costs roughly the current window `W` plus its own
input/output. Narrowing wins when `R*D > W + extra interaction bytes`. For the cat
example `D=18,434`, so at `R=71` the break-even extra-turn cost is approximately
1,308,814 bytes; for grep it is 749,405 bytes. The supplied 745 MB/24 h aggregate
does not determine `W` at an individual decision, retention, or follow-up reads.
4 KiB is a candidate experiment setting, not an established optimum.

**Ship versus drop:** retain bounded batching and the existing narrow sed guard;
consider an opt-in, post-output Bash notice using the existing channel, then
measure subsequent output bytes, extra turns and task completion before enabling
it. Do not ship this four-idiom blocking extension: the largest sample gains
come from asking for less content (cat and grep), while equivalent-range awk and
head/tail substitutions add bytes and may require continuation turns. A generic
range-awk detector and head/tail guard are poor maintenance trades; drop them.
Whole-file cat and broad-context grep are candidates for advisory targeting,
not new blocks without prevalence/false-positive evidence. The supplied probe
puts grep at only 7% and does not quantify awk/cat/head-tail; it cannot establish
fleet savings for this extension. Investigate `file` separately (28% of measured
text) rather than inferring benefit from a parser whitelist. A notice is cheap
to dispatch but has zero immediate byte savings and no demonstrated behavior
change; do not confuse its implementation feasibility with effectiveness.

## Validation

`soldr cargo test -p clud --lib block_bad_cmd_hunt -- --test-threads=1`:
20 passed, 0 failed (18 existing + 2 spike tests). Removed its sole warning,
an unused prototype import. `git diff --check` passes. `bash lint --host-only` was stopped at the spike time box during Clippy
(dependency cache misses; no reported diagnostics): **lint incomplete**, not green.
Formatting and Ruff stages reported success. Private full lint log:
`/home/niteris/.clud/tmp/tool-output/local-test-private/local-test-iCPT9u.log`.
No hosted validation, push, PR or merge is part of this spike.

# Shell result budget: production-boundary replay

Measured 2026-11-03, subscription-only; no model/provider calls, subagents,
embeddings, or reranking. This is a captured-output replay through the compiled
production `codex_translate::translate_bytes` entrypoint, not live model task
completion or a native Codex shell-tool session.

Re-executed the same 40 shell commands from the Round 6 20-task benchmark in
`/home/niteris/dev/clud-wt-hunt`, after verifying every gold source SHA-256.
Raw outputs total exactly 44,863 bytes, matching Round 6. Fed those strings as
Bash tool results into the production translator using the ignored
`codex_translate::tool_result_cap_translate_tests::replay_shell_results` test.
Scored source-line identity with Round 6's `positions` scorer and frozen gold
provenance. The benchmark commands and scorer are campaign-checkout artifacts,
not dependencies of the shipping implementation.

| Task | Raw bytes | Production emitted bytes | All anchors raw / capped |
| --- | ---: | ---: | ---: |
| dispatch | 1,896 | 1,896 | 1 / 1 |
| decode | 2,029 | 2,029 | 1 / 1 |
| notice | 1,050 | 1,050 | 1 / 1 |
| matcher | 1,896 | 1,896 | 1 / 1 |
| hunt | 1,288 | 1,288 | 1 / 1 |
| home | 865 | 865 | 1 / 1 |
| purge | 1,292 | 1,292 | 1 / 1 |
| flags | 1,014 | 1,014 | 1 / 1 |
| skills | 1,337 | 1,337 | 1 / 1 |
| hooks | 1,361 | 1,361 | 1 / 1 |
| classify | 4,683 | 4,683 | 1 / 1 |
| short-form | 1,996 | 1,996 | 1 / 1 |
| marker-priority | 2,501 | 2,501 | 1 / 1 |
| config-conflict | 3,317 | 3,317 | 1 / 1 |
| config-overlay | 1,261 | 1,261 | 1 / 1 |
| origin | 2,288 | 2,288 | 1 / 1 |
| completion-trace | 4,473 | 4,473 | 1 / 1 |
| escape-trace | 6,280 | 5,262 | 0 / 0 |
| ci-tiers | 1,986 | 1,986 | 1 / 1 |
| ci-queue | 2,050 | 2,050 | 1 / 1 |

Totals: **44,863 → 43,845 bytes**, saving **1,018 bytes (2.27%)**.
Task survival: **19/20 → 19/20**. Individual anchors: **21/22 → 21/22**;
all 21 raw-visible anchors survive. Only the escape-trace sed call is oversized;
that task already misses a gold anchor in the raw arm. No additional missing
anchors imply no additional oracle recovery calls; autonomous recovery was not
run. Round 6's prototype emitted 43,771 bytes; production's marker is 74 bytes
longer (executable clud prefix and the explicit payload-plus-marker warning).
Path length contributes to marker size, so totals can vary across machines.

Setting `CLUD_TOOL_RESULT_MAX_BYTES=0` for a separate replay was checked against
all 40 raw strings byte-for-byte. The reproduction seam accepts any JSON array
of UTF-8 output strings; see [configuration and invocation](../docs/architecture/codex-via-claude.md#shell-tool-result-budget).

Local artifacts outside git:

- `/home/niteris/.clud/tmp/tool-cap-replay-fixture.json`: frozen gold identities,
  commands and raw artifact pointers from the original campaign.
- `/home/niteris/.clud/tmp/tool-cap-replay-input.json`: re-executed UTF-8 results.
- `/home/niteris/.clud/tmp/tool-cap-replay-output.json`: production translation.
- `/home/niteris/.clud/tmp/tool-cap-replay-disabled.json`: disabled translation.
- `/home/niteris/.clud/tmp/tool-cap-replay-report.json`: per-task scoring.

Round 5's replacement corpus held 6,664 results / 9,781,225 UTF-8 text bytes;
the top decile held 5,917,067 (60.49%). Its hypothetical hard all-tool cap leaves
6,583,683 (32.69% saved). Round 6's literal shell-only simulation leaves
7,252,708 total bytes (25.85% saved). **These are historical counterfactuals,
not new production-route measurements.** Native Codex, direct Claude/provider
routes and unknown originating call IDs bypass the translator. No new full
corpus replay, middle-of-output anchor study, live model completion, cache-hit
study, net recovery-cost measurement, native platform/integration validation,
or disk-retention/load study was performed.

---
name: clud-gate
description: Run lint and tests through clud gate with compact results and per-element logs. Use when validating code changes in a repository.
triggers:
  - When lint or tests are needed to validate repository changes
disable-model-invocation: false
---
<!-- managed-by: clud -->

Run lint and tests only through `clud gate <lane>`; the direct invocations are discouraged and will be refused after the first advisory.

Choose `clud gate lint` for lint, `clud gate test` for tests, or `clud gate both` for both. Run from the repository directory. The gate detects the supported Rust, Python and Node checks; Rust commands go through soldr.

Read the compact JSON result. On failure, inspect the failing element's `log_path` with a bounded read, fix the cause, and rerun the lane. `both` stops at the first failed element. Report the verdict and any validation gaps; an unsupported or missing runner does not prove a check passed.

For a necessary direct debugging invocation, `CLUD_GATE_BYPASS=1` opts out of the advisory guard. The guard otherwise advises on the first distinct command shape, refuses the second once with the lane replacement, then stays silent. Identical retries pass. Missing binary or history and unknown shell shapes also pass silently.

For code changes, preserve RED -> GREEN: establish a failing focused test or repro, implement the fix, then use the gate to verify the applicable lane.

# CI architecture — build-once / run-everywhere

Status: implemented (supersedes the 24 per-platform leaf workflows + `_lint.yml` /
`_unit-test.yml` / `_integration-test.yml`).

## Current CI selection

The sole push/PR workflow is `.github/workflows/ci.yml`. Ordinary PRs and
`main` pushes run static checks plus the Linux x64 build and unit suite.
The literal `ci-test` PR label adds Linux x64 integration, the real-harness suite and Windows x64.
Linux integration stays in `ci-full` and release validation too;
it was moved out of ordinary runs after the measured minimal lane exceeded
the 12.5% runner-minute budget.
Every PR runs Linux Dylint for the custom late lint: a host pass plus
parallel check-only cross-target passes for `x86_64-pc-windows-msvc` and
`aarch64-apple-darwin`. Full local `bash lint` runs the host Dylint pass after
Clippy. `ci-full` (and
existing `ci:full` labels) and manual full runs select all
six build targets. A manual run requires a reachable
`candidate_sha`; its CI workflow and helper tree must match the selected
branch's workflow revision, so the same candidate can be retried after
unrelated `main` changes. Every checkout uses that SHA. The required `CI OK`
check verifies every job selected by the mode. Adding or removing a label
reruns selection on the PR head SHA.

Linux x64 Clippy runs as its own job (`lint-linux-x64`) beside the build, not
inside it: it produces nothing the unit lane consumes, so as a step of the
build job it only delayed the unit suite by ~41 s. It shares the build job's
zccache namespace read-only (`save-cache: false`) so the two jobs never race to
write one immutable cache key. Windows and macOS clippy stay inside their
build jobs (they run only in `ci-test`/`ci-full`).

The Linux x64 unit suite runs as four parallel jobs (`test-linux-x64-unit`, a
matrix over `LINUX_X64_UNIT_SHARDS` in `ci/ci_matrix.py`): the Rust harnesses
(`rust`) and each third of the pytest suite (`py1of3`..`py3of3`, split by file
with recorded weights in `ci/unit_shard_weights.json`, `ci/pytest_shard.py`).
It was one ~190 s job: ~55 s of harnesses, then ~135 s of serial pytest. The
suite is CPU-bound (every test launches real `clud` processes) and a hosted
"4 vCPU" runner is two physical cores, so in-process parallelism does not help
(pytest-xdist measured slower, 172 s against ~135 s serial, with a 2.5x
per-test slowdown from contention); separate machines do not contend, so the
split costs only the extra jobs' setup. Refresh the weights from a run's `logs/pytest-unit.xml` when
the balance drifts, or from the shard jobs' timestamped logs (`gh run view
--log --job`: each pytest progress line's timestamp minus the previous one's
is that file's time). Average at least two runs. A stale or missing entry
only costs balance, never coverage. All other lanes run the whole suite in one job (`--shard all`).

### A release recomputes its own proof (#2202)

The release workflow's `full-ci-gate` job **runs** `ci.yml` on the exact
tagged SHA, inside the release run, as a reusable-workflow call with
`candidate_sha`. A non-empty candidate resolves the full tier (all six
triples, Dylint, unit and integration) exactly as a manual dispatch does, and
`CI OK` fails the job unless every selected job passed. No release build or
publisher starts until it succeeds. A routine `main` version change does not
trigger the release.

A release consults no earlier evidence: no local-gate attestation, no
default-branch reuse decision, and no earlier CI run. The reuse steps in
`ci.yml` key only on a `main` push; a call from a tag push is not one,
and `tests/test_release_gate.py` pins that. This
replaced a lookup of a separately dispatched `CI full <sha>` run, which
failed any tag pushed without that manual step. It is the fleet rule
REL-007 in forge-ci/ci.yml `docs/policy-general.md`.

The gate establishes complete dev-profile CI execution, not release wheel
execution: release-profile artifacts are built after it. The
target/label policy lives in `ci/ci_matrix.py`; the workflow has no preceding
matrix-planning runner job.

## Local validation before remote CI

GitHub Actions is the CI of record. Run focused tests or lint locally when
useful, then push and require every selected hosted check to pass. Bosn is
optional: publishing and PR validation require no local receipt or commit
trailer. PRs and merge groups execute their selected hosted jobs.

### Two local test tiers (#2356)

| Tier | Command | Runs | When |
| --- | --- | --- | --- |
| 0 | `bash test --host` | `soldr cargo test --workspace --lib -- --skip tier1_` plus pytest `-m "not integration and not daemon"` | Constantly, on the host |
| 1 | `bash test` / `bash test --integration`, or `clud ci` | The `integration` test target, which spawns a built `clud`, plus every `daemon`-marked Python test | After Tier 0 passes, at most once per PR before push, preferably in bosn |

Tier 0 builds no workspace binary and starts no daemon, so it cannot touch
the installed `clud` or its daemon. It keeps the host's build cache warm
across edits. A pilot on 2026-10-29 measured the full bosn PR plan at
26.7 min cold and 9.5 min warm, mostly test execution, which no cache removes.

`tests/conftest.py` marks every test in a module that refers to a built
binary (`CLUD_TEST_BINARY`, `CLUD_TEST_MOCK_AGENT_BINARY`,
`CLUD_TEST_BLOCK_BAD_CMD_BINARY`, `target/debug`) as `daemon`. Tier 0 points
those variables at a path that does not exist. An unclassified test that
runs a binary therefore fails loudly instead of quietly reaching the
host's `clud`; the fix is to refer to the binary through one of those
variables, or to mark the module `daemon` explicitly
(`pytestmark = pytest.mark.daemon`). On the Rust side, the `integration`
harness is Tier 1 because Tier 0 runs `--lib` only. A library test that needs
a built binary is named with a `tier1_` prefix, and Tier 0 passes
`--skip tier1_` (the `daemon::http` turn tests that drive `mock-agent`
are an example). GitHub Actions still runs both tiers and remains the CI of
record.

For an isolated local replay, `bosn ci` runs supported workflow jobs under
[act2](https://github.com/zackees/act2), with a snapshot of the checkout,
including uncommitted work. It cannot prove native behavior on another OS.

**For optional replay, use `clud ci`** (#1839). It wraps the same bosn run, waits, and prints a
one-line verdict plus only the failing steps' diagnostics (rustc/clippy errors
with their `-->` location, failing tests and panics, ruff findings),
ANSI-stripped. An `incomplete` run or engine error is reported as a failed run
even when the observed jobs succeeded; missing coverage or failed cleanup
cannot supply a pass. This diagnostic command does not issue attestations:
the shared CI tool verifies eligibility separately. A stale bosn daemon gets
a one-line fix instead of a refusal.

```bash
clud ci                                          # the PR plan
clud ci static-checks                            # formatting, ruff, guards
clud ci lint-linux-x64                           # clippy and doc-tests
clud ci test-linux-x64-unit --filter openrouter_free   # plus matching test lines
clud ci test-linux-x64-unit --json               # verdict, failing steps, diagnostics
```

The raw bosn commands below remain available:

```bash
docker info
bosn ci run --workspace . --trigger pr --wait              # the PR's jobs: static, build, clippy, dylint, Linux unit
bosn ci run --workspace . --trigger pr --job static-checks --wait # formatting, ruff and static guards
bosn ci run --workspace . --trigger pr --job lint-linux-x64 --wait # Clippy and doc-tests
bosn ci run --workspace . --trigger pr --mode test --wait  # adds the ci-test lanes act can run (Linux integration)
bosn ci report RUN                                         # verdict and the first failure
bosn ci logs RUN --follow                                  # live output; --job/--step narrow it
```

`--trigger pr` builds the same `pull_request` payload GitHub would, and
`--mode test`/`full` select the job set that the `ci-test` and `ci-full` labels
select. Jobs that need a native runner (Windows, macOS, arm) are reported
`unsupported`, not run.

**Stock workflows, no local special cases ([zackees/ci.yml ACT-003](https://github.com/zackees/ci.yml/blob/main/docs/policy-general.md)).**
`.github/` runs under act2 exactly as written. act2 sets
`RUNNER_ENVIRONMENT=github-hosted`, so actions' hosted defaults (setup-uv's
cache) engage. It also runs job containers with an init that reaps orphans, so
daemon shutdown tests see exited PIDs. bosn's own workflow rewrites
(checkout localisation, output flushing) go to an overlay act reads, never
into the tree a job checks out, so tests that read `.github/` see this repo's
files. `ACT=true` is the local-runner signal: actions read it to tune their
caches (setup-soldr's `save-cache: auto` saves fully, and on `v0` since
setup-soldr#563). A workflow reads it only under a tracked exception;
`tests/test_ci_matrix.py` fails on any `env.ACT` without a `zackees/ci.yml#`
link above it. The one exception today is setup-uv's `prune-cache`
([ci.yml#226](https://github.com/zackees/ci.yml/issues/226)). A new local gap
gets an issue in zackees/ci.yml cross-referenced with zackees/act2 and
zackees/bosn, not a workaround here.

Caches live in bosn's machine-scoped cache volume (the Actions cache server,
fetched actions, the runner image), so the second run of a job restores what
the first saved. Each run gets its own engine, so concurrent runs from any
checkout or session never share containers, names or an action checkout.

### Local CI budget

`bosn ci` needs bosn 0.1.10 or newer (`uv tool install 'bosn>=0.1.10'`; check
with `bosn --version`). Older releases run stock nektos/act, which lacks the
runner parity and overlay above, or an act2 that rejects `concurrency.queue`
and cannot run the Pages jobs. Each engine's build storage is a disk-backed
volume sized from free disk (zackees/bosn#425), so clud's PR run needs no
`storage_gib` override.

A full minimal PR run takes about 25 minutes cold and less warm. Every clud
session on the machine shares one bosn daemon and its CPU, so each extra run
slows every other one. On 2026-10-02 a one-module fix spent about 50 minutes
in the old local act path (#1715). So:

- **If using local replay, run the full PR plan at most once per change**.
  Don't loop it. `--job static-checks` (a few minutes) is fine for formatting and
  lint. RED can come from that one local run or from the PR's first CI run.
  GREEN comes from the PR's CI, which is the CI of record: after a fix, push
  and watch the PR rather than rerunning locally.
- **Don't wait in the bosn queue.** If `bosn ci show RUN` stays `queued` for
  2 minutes (`bosn ci runners` shows every slot busy), the daemon is
  saturated. Cancel your run, push, and write "local CI skipped: bosn daemon
  busy" in the PR.
- **Never restart a running run because you made another commit.** Let it
  finish, or cancel it, then push.
- **Stop only your own run** with `bosn ci cancel RUN`. Never use `pkill -f`
  or `kill $(pgrep -f bosn …)`: the pattern matches every session's run.

For installer changes, run the focused jobs:

```bash
bosn ci run --workspace . --workflow .github/workflows/installer-check.yml --job resolver-unit --mode full --wait
bosn ci run --workspace . --workflow .github/workflows/installer-check.yml --job catalog-unit --mode full --wait
```

The published-candidate lane runs one matrix leg against the version in
`pyproject.toml`, once that candidate is published
([zackees/bosn#430](https://github.com/zackees/bosn/issues/430)):

```bash
V=$(sed -n 's/^version = "\([0-9][^"]*\)"/\1/p' pyproject.toml | head -n 1)
bosn ci run --workspace . --workflow .github/workflows/installer-check.yml --job public-host \
  --event workflow_call --input release_tag=$V --input mode=candidate \
  --matrix target:x86_64-unknown-linux-musl --env PYTEST_ADDOPTS=-s --wait
```

It requires a passed pytest and matching public evidence. The release workflow
still checks every required host and guest before promotion.

The Pages build jobs run locally too (zackees/bosn#438): bosn stubs
`actions/configure-pages` with the project site's URL, and
`upload-pages-artifact` uploads to act's artifact server. Only the deploy job
is GitHub-only.

```bash
bosn ci run --workspace . --workflow .github/workflows/install-pages.yml --job build-site --trigger pr --wait
bosn ci run --workspace . --workflow .github/workflows/model-pages.yml --job build-site --trigger pr --wait
```

Focused tests and lint may run directly on the host; Rust commands still
go through soldr. For optional workflow replay, run the job containing the test. act cannot execute native Windows
or macOS. For changes to Windows platform implementations or OS contracts, use the
`ci-windows` PR label for the Windows
build, unit and integration suites after the local run, and keep Windows PTY
tests enabled. A `ci-windows` run still includes the
routine Linux x64 lanes but no macOS
([What protects `main` today](#what-protects-main-today)).

`act --dryrun` is useful for planning/validation but does not run action code.
Its Docker runner is not a native macOS or Windows runner, and this workflow's
reusable jobs, artifacts, and runner environment can differ from GitHub's.
`act` creates Docker resources outside Bosn's managed registry.
Run only a representative supported job locally; do not claim that it replaces
the remote matrix. If `act` cannot model the affected path, use the relevant
native GitHub Actions lane and record what was and was not reproduced locally.

The workflow review that motivated this rule caught a real false positive: a
cache-looking input was accepted by a policy checker but did not enable the
cache in the **pinned** `setup-soldr` action. When editing third-party action
inputs, inspect that action's `action.yml` at the exact ref used by the
workflow, verify the input's effect rather than just its spelling, and add a
negative/mutation case to any policy checker. If a job approaches its timeout,
measure the slow step or cell and split work where possible instead of simply
raising the timeout.

If Docker or Bosn is unavailable, use focused host checks and hosted CI.
Do not auto-install tools or prune Docker resources. See the bundled `clud-bosn` skill for prerequisites.

### Attested skip (GATE-008/010)

**Disabled as a hosted CI prerequisite and skip mechanism.** An unstamped PR
runs every selected hosted job. `CI OK` still uses the pinned shared verifier
to check actual job outcomes and independently verify default-branch reuse.
Missing, failed or unjustifiably skipped required jobs remain failures.

`local-gate.toml` and `ci-attestations.yml` remain available for optional
local diagnostics. Both gate modes are `shadow`; CI does not invoke
`local-gate verify`, consume local skip outputs, or pass `--attested-workflow`
to its final verifier. Local receipts do not waive hosted checks. The
historical enrollment measurements below describe the retired behavior.

#### Validation evidence (2026-10-03)

The warm local gate on tree `586af3c573ea` completed in **419 s** using
bosn 0.1.10 and act2 `0.2.89-act2.2` (run
`81614bb8-d59f-4a22-b5d0-2694d236b5ae`). Static checks, Clippy/doc-tests,
all three Dylint target passes, the build, the Rust and both Python unit
shards, and `CI OK` executed successfully. The gate wrote `Local-Gate:` and
all seven `Ci-Attestation:` trailers. This is one warm measurement, not a
cold-build estimate or a native Windows/macOS test result.

[PR #1781](https://github.com/zackees/clud/pull/1781)'s
[remote run](https://github.com/zackees/clud/actions/runs/37109292374)
accepted that stamp but reran the routine jobs because the workflow changed
(`surface-changed`); it passed in 5 min 37 s. An earlier local baseline
failed during Docker container removal before unit execution
([ci.yml#250](https://github.com/zackees/ci.yml/issues/250)) and earned no
stamp. A successful build alone does not justify attesting a skipped suite.

[The ordinary attested PR #1782](https://github.com/zackees/clud/pull/1782)
[passed in 20 s](https://github.com/zackees/clud/actions/runs/37110407536):
all five routine job groups skipped, and `CI OK` accepted their verified
local proof. Its own local gate had executed every selected check in 473 s.

When inspecting `bosn ci report`, also inspect the expanded matrix children
in `bosn ci show RUN` and their logs: the report can list the unexpanded
`test-linux-x64-unit` planning node as skipped even when its three children
executed. Require each actual shard and `CI OK` to pass.

### Default-branch reuse evidence (GEN-021)

The `CI mode` job probes `main` pushes with the pinned checker's
`ci-lint reuse-check --mode enforce`. It compares the merged tree with its
associated PR head and requires a successful PR run no older than 12 hours
containing all seven routine check cells: static checks, Dylint, Clippy,
build, and the three unit shards. Attested PR jobs that were skipped do not
count as successful executed jobs for this probe.

A verified decision skips only Static checks, Clippy and the Linux unit
caller. Dylint and Build linux-x64 still run as default-branch cache writers.
API errors or missing proof produce no reuse. PR and release events do not
run the probe.

The mode job exports the existing schema-1 decision as `reuse_evidence`,
using `reuse-check`'s `evidence_json` output. `CI OK` supplies this document
to `ci-lint gate --default-branch-reuse` alongside the actual needs results
and GitHub event. The shared consumer derives every selected reusable leaf's
hosted name from workflow source, then verifies distinct proving job IDs
against their live name, head SHA, run ID and successful conclusion. Missing,
ambiguous or mismatched evidence refuses the skip. Local attestations do not
provide a hosted skip path.

The mode job and `CI OK` record the reason, tree and proving PR/run in their
summaries. Default-branch reuse enforcement predates PR #1887, which
introduced the enforced local prerequisite. PR #1889 migrated the final
consumer to the shared verifier and passed hosted run `37699034850` with
all selected jobs executing because its CI surfaces changed. That historical enrollment is no longer a prerequisite for a PR.

### Manual Windows probes (ignored tests)

<!-- manual-windows-probes -->

Three Windows-only test files are `#[ignore]`d, with a "Run manually" note in
their module docs, because they
pin CPU cores for wall-clock seconds or time host-wide enumeration. They are the
only tests that exercise the real Win32 sampling APIs (`Toolhelp32Snapshot`,
`GetThreadTimes`, `GetProcessIoCounters`, `NtQueryInformationProcess`,
`NtQuerySystemInformation`, sysinfo's `ProcessesToUpdate`) behind the wedge
watchdog, the reaper's Job-Object diagnostics and process tier refresh. No CI
lane passes `--ignored`, so nothing runs them automatically
([#1368](https://github.com/zackees/clud/issues/1368)).

Run them on a real Windows box before cutting a release, and whenever you touch
wedge watchdog, reaper diagnostics or process tier refresh code, or bump the
`windows` / `sysinfo` crates. They are modules of the one `integration`
test target (#1726), so the filter is the probe's module path:

```bash
soldr cargo test -p clud --test integration reaper::platform::windows::wedge_watchdog_e2e -- --ignored --nocapture --test-threads=1
soldr cargo test -p clud --test integration diagnostics::platform::windows::win32_hooking_probe -- --ignored --nocapture --test-threads=1
soldr cargo test -p clud --test integration diagnostics::tier_refresh_probe -- --ignored --nocapture --test-threads=1
```

Any new test file that is `#[ignore]`d and says "run manually" must be added to
this list; `tests/test_manual_windows_probes.py` enforces it.

### Reading CI status without flooding context (#2312)

This section owns the rule; skills link here. Every CI read re-enters the
agent's context, and hand-polling repeats near-identical output.

- To **wait** on CI, use `clud tool run github/pr_merge_watch.py -- <PR>`
  (background or bounded, never a hand-written loop; DD-168). Its output is
  already compact.
- For a **one-off** read, print one summary line of counts, then only what is
  unresolved:

  ```bash
  gh pr checks N --json name,state --jq '(group_by(.state)|map("\(.[0].state // "pending")=\(length)")|join(" ")), [.[]|select(.state!="SUCCESS" and .state!="SKIPPED")|.name]'
  gh run view ID --json jobs --jq '(.jobs|group_by(.conclusion)|map("\(.[0].conclusion|if .=="" or .==null then "pending" else . end)=\(length)")|join(" ")), [.jobs[]|select(.conclusion!="success" and .conclusion!="skipped")|.name]'
  ```

  The first line is the per-state count (for example
  `SUCCESS=14 SKIPPED=6 FAILURE=1`), so you can still see which jobs ran
  without listing the passing ones. The second line lists only the
  unresolved job names. An empty list means nothing is unresolved. Drop the
  filter only when you need the full job list.
- Do not repeat an identical read; a status that has not changed is not new
  information.

## The problem

Every push fans out to **12 heavy workflows** (6 platforms x {unit-test,
integration-test}), each of which is a *from-source native build*:

| Workflow | Full workspace compiles it performs |
| --- | --- |
| `_unit-test.yml` | `cargo clippy --workspace --all-targets` (1) + `cargo build -p clud -p mock-agent` + `cargo test --workspace --no-run` (1) |
| `_integration-test.yml` | `cargo build -p clud -p mock-agent` + `maturin build` dev wheel (1) |

So per push: **~12–18 full workspace compiles**, spread across **12 mutually
invisible cache namespaces** — every job pays its own cold-cache tax and none
of them warms another. Four of the six
platforms are macOS/Windows runners, which are the scarcest and slowest in the
pool, so the fan-out converts directly into queue depth.

On top of that, three genuinely platform-independent checks run **six times
each**: `ruff`, `cargo fmt --check`, and `ci/banned_imports.py`
(`ci/lint.py:37-43`).

## The shape of the fix

Split the two things CI conflates — *producing artifacts* and *executing them* —
and make the producer side live on Linux.

```
  ┌──────────┐  ┌──────────┐   per triple, independently:
  │  static  │  │  dylint  │
  │  ubuntu  │  │ 3 native│   ┌──────────────┐   bundle-<triple>   ┌────────────┐
  │ ruff/fmt │  │  hosts   │   │ build-<trip> │ ─────────────────►  │ test-<trip>│
  │ /banned  │  │all modes │   │  ubuntu-24   │  .tar.gz artifact   │   NATIVE   │
  └────┬─────┘  └────┬─────┘   │  clippy +    │                     │  unit +    │
       │             │         │  bins +      │                     │ integration│
       │             │         │  test bins   │                     │ no cargo,  │
       │             │         │  + wheel     │                     │ no rustc   │
       │             │         └──────────────┘                     └─────┬──────┘
       └─────────────┴────────────────────────────────────────────────────┤
                                                                          ▼
                                                                   ┌────────────┐
                                                                   │   ci-ok    │
                                                                   └────────────┘
```

Each triple gets its **own** build job and its **own** test job, rather than one
build matrix feeding one test matrix. That is not stylistic: `needs:` on a
matrix job is all-or-nothing in GitHub Actions, so a single `test` matrix
depending on a single `build` matrix would make the Linux tests — ready first —
wait for the slowest cross-build in the set. GitHub exposes no per-leg
dependency edge, so the lanes are written out longhand. `ci/ci_matrix.py`
remains the source of truth for the triple table and
`tests/test_ci_matrix.py::test_ci_yml_covers_exactly_the_targets_table` fails if
the YAML drifts from it.

There is deliberately no separate `plan` job. The `static` job (`CI mode`)
resolves the mode before builds begin, so invalid labels or a mismatched
dispatch SHA fail before allocating cross-build runners. It installs nothing
and runs no lint, so it does not delay them either; the lint is the parallel
`Static checks` job.

Three structural claims, in the order they matter:

1. **One build per triple, not three.** `clippy --all-targets`, the workspace
   binaries, the `cargo test --no-run` harness binaries, and the dev wheel are
   produced in *one job with one `target/` directory*. They already share
   ~95% of their compilation graph (every dependency rlib); today that graph
   is recompiled on three separate machines. This is the single largest win
   and it requires no cross-compilation at all.
2. **The build host is always Linux.**
   Linux runners are the cheapest and least contended, and — critically — all
   targets then share one runner class, so cache behaviour is uniform.
3. **macOS/Windows test runners never compile product artifacts.** They download a bundle and execute
   it. Their job duration collapses from "cold C++ build + test" to "test",
   which is what makes using them sparingly viable.

## Target tiers — using scarce runners sparingly

The `ci-windows` label is an iteration mode for Windows-only work (#1310): it
runs static checks plus the Windows x64 build and both Windows suites, plus
every routine (`minimal`) Linux x64 lane: Dylint with its Windows and macOS
cross-target passes, clippy, build and the unit shards (#1652). It skips Linux
integration, harness and the other targets. `CI OK` passes only when all of
those lanes pass; `ci-test`/`ci-full` take
precedence. The merge queue runs only the `minimal` lanes; see
[What protects `main` today](#what-protects-main-today).

`ci-macos` is the same iteration mode for macOS: every routine Linux x64 lane
plus the build and suites of both Darwin triples (`aarch64-apple-darwin` on
`macos-15`, `x86_64-apple-darwin` on `macos-15-intel`), and nothing else
native. Use it for macOS-only evidence instead of paying for ci-full's Windows
and Linux ARM lanes. `ci-windows` and `ci-macos` together are rejected; that
combination is `ci-full`.

`ci/ci_matrix.py` defines the target inventory consumed by the workflow. Not
every push needs all six targets.

| Tier | Triples | Trigger |
| --- | --- | --- |
| `minimal` | Linux Dylint (host + Windows/macOS cross-target) + `x86_64-unknown-linux-gnu` build and unit suite | ordinary PR and `main` push |
| `extended` | minimal + Linux x64 integration + harness + `x86_64-pc-windows-msvc` | PR labeled `ci-test` |
| `windows` | minimal + `x86_64-pc-windows-msvc` build, unit and integration | PR labeled `ci-windows` |
| `macos` | minimal + both Darwin triples' build, unit and integration | PR labeled `ci-macos` |
| `full` | extended + `aarch64-unknown-linux-gnu`, `aarch64-pc-windows-msvc`, both Darwin triples | PR labeled `ci-full` or legacy `ci:full`, source-pinned manual dispatch, the nightly `schedule` run |

`ci-test` covers Linux and Windows product build/tests. Both hosted macOS
architectures run product tests only in `ci-macos`, `ci-full` and release validation;
the macOS Dylint cross-target pass runs in every mode. Routine events use
Linux x64 for fast feedback. Nothing runs the complete matrix before merge
unless the PR carries `ci-full`; see below.

### When to add native CI labels

`ci-full` is for changes involving platform implementation code, not business
logic calling platform code. Inspect the diff, not the filename: native
API/FFI handling, platform adapters/selectors, changed OS branches and
platform-specific runtime contracts need the affected native lanes. Shared
orchestration, stream parsing, helper extraction and calls to unchanged
platform APIs use routine CI. Moving an existing OS branch unchanged into a
helper is not a platform implementation change.

Before adding a label, identify the changed implementation and the native
behavior the selected lanes validate in the PR body. Use `ci-windows` for
Windows-only implementation changes, `ci-macos` for macOS-only ones, and
`ci-full` for platform changes requiring the complete matrix. An incomplete local test run does
not justify expanding coverage: routine remote CI supplies that evidence.

PR #1769's shared pump helper extraction is the negative example: it called
existing platform code and preserved OS branches, so it should have used
routine CI. Changing termios, ConPTY, native console handling or the OS
contract itself is a positive example. Release/candidate full validation
is unchanged. The matrix script honors explicit labels; it does not classify
source diffs, so agents and reviewers apply this selection rule.

### What protects `main` today

Since 2026-10-24 (#2048, #2125) the repository lives at `talos-hq/clud`,
where `main` has a merge queue (ruleset 25798020, squash, `ALLGREEN`, up to
5 entries per group) and branch protection requires `CI OK`. Repository
admins can bypass the queue. So:

- PR CI tests the PR head SHA; the queue then tests the merged tree in a
  `merge_group` run before it lands.
- The label picks the PR's lanes. Every mode, `ci-windows` included, requires
  the routine Linux x64 lanes on the head SHA before `CI OK` is green
  (#1652); macOS runs only under `ci-full`.
- A `merge_group` run selects the `minimal` tier, the same lanes as a `main`
  push (#2125). Full native coverage stays on the nightly run, `ci-full`
  labels and release validation; a full queue tier would make every merge
  wait on all six targets.
- The `push` run on `main` still runs `minimal` and still saves the caches
  PRs read.
  Its concurrency group is fixed (`main-push`, never cancelled): one run is
  active and only the newest pending push waits behind it, so a burst of
  merges cannot stack parallel `main` runs (#2048, as obs-rust/obs-studio's
  push build). The queue already tested each merged tree; nightly and
  SHA-pinned dispatch runs on `main` keep their own `run_id` groups.
  Its `Default-branch reuse` step passes `--queue-evidence`: when the merged
  tree differs from the PR head's (the queue merged a PR that was behind
  `main`), the green `merge_group` run on that exact commit proves it, so
  the push skips the lanes the queue just ran.
- A queue ref names its base commit (`gh-readonly-queue/main/pr-<N>-<sha>`),
  so when `main` moves GitHub starts a new run per queued PR that no
  concurrency group pairs with the old one. The `cancel-superseded` job
  (`ci/cancel_superseded.py`) cancels every older in-flight queue run for the
  same PR.
- A nightly `schedule` run (`41 6 * * *`, #1849) runs the `full` tier on
  `main`'s head, so a native-only regression surfaces within a day instead
  of waiting for an unrelated `ci-full` PR. It never skips or reuses a lane:
  reuse is computed only for main pushes, and the `Nightly runs skip nothing`
  step in `CI mode` fails if it is ever set on a schedule. The `nightly-report` job, the only one with
  `issues: write`, opens or comments on a `ci-nightly-failure` issue when
  `CI OK` is anything but `success` (cancelled counts as red), and closes
  it on the next green night. Cost: about 200 runner-minutes per night.

#### `ci-windows` keeps the routine Linux lanes (#1652, decided)

`windows` mode runs every `minimal` lane on the same run, and `CI OK`
requires them through the shared verifier's source-derived workflow plan.
A green `CI OK` requires each selected lane to have passed on that head SHA
or carry a valid independently verified proof. Rejected options:

- Look up another run for the same SHA from `CI OK`. That adds a GitHub API
  dependency and ordering races: the other run may still be queued, skipped
  or cancelled. #1639 was this bug class (skipped/queued runs counted as
  passing), and `pr_merge_watch` stops at the first red `CI OK`, so a gate
  that fails "not yet" would abort the watch.
- Run only Linux build + the Rust unit shard. That saves ~3 runner-minutes
  (the two pytest shards) but leaves the Python suite and clippy unchecked,
  which a routine run would have caught.

Cost, measured on `main` minimal runs 36764898391 / 36770722764: the added
Linux lanes are ~9.7-9.9 runner-minutes (clippy 1.7, build 3.5-3.8, unit
shards 1.2-1.7 each) and ~6 minutes of wall-clock. They run beside the
Windows build/test, which is longer, so the label's feedback time does not
change. The #1310 goal of fast Windows iteration still holds; the label now
spends ~10 Linux minutes per push to keep `CI OK` honest.

#### Merge queue (#2048, decided)

Enabled on 2026-10-24 after the move to the `talos-hq` organization: GitHub
offers merge queues only on organization-owned repositories. The queue is
the only thing that tests the merged tree rather than the head SHA
([DD-132](../DESIGN_DECISIONS.md#dd-132-there-is-no-merge-queue-the-ci-windows-rationale-in-dd-088-is-corrected)).

For a GitHub PR, `clud tool run github/pr_merge.py -- <PR> --repo <OWNER/REPO>
--match-head-commit <full-head-SHA>` reads GitHub's merge queue for the actual base branch. It enqueues a queue-protected PR using the GitHub GraphQL
`enqueuePullRequest` mutation, which works even when repository auto-merge is
disabled; otherwise it invokes a normal squash merge. It checks the live head
against the required SHA and never uses admin bypass. A successful enqueue is
not a merge: follow it with `clud tool run github/pr_merge_watch.py -- <PR>
--repo <OWNER/REPO> --until merged` and verify the MERGED verdict. This mode
reports queue position and state changes, exits 13 for ejection, 14 for an
entry that remains UNMERGEABLE for two polls, and 4 on timeout. Without
`--until merged`, the watch retains its ordinary check and merge-wait behavior.
On timeout or API failure,
inspect the PR and queue state before retrying the mutation (#2235).

#### Agent landing: workers end at enqueue, chaperones own the merge

This section owns the contract; `LAND_CONTRACT`, `launch-plan.md`, `CLAUDE.md`
and [DD-192](../DESIGN_DECISIONS.md#dd-192-workers-end-at-enqueue-and-a-chaperone-owns-the-merge)
(rationale and timing evidence) link here. It is a candidate fleet policy,
[GEN-033](https://github.com/forge-ci/ci.yml/blob/main/docs/agent-guide.md).

**Scope.** The chaperone handoff applies only where a merge queue is required
and the harness can run background agents. With no required queue, or without
background agents, the agent observes the PR until it merges, per repository
policy (a repository with no hosted required checks merges after the local
gate). One checkout per worker; parallelism comes from cheap chaperones, not
worktrees.

- **Worker:** fix, test, lint, push. Apply the repository's native-coverage
  labels, if any, at creation with `gh pr create --label`, never later. Wait
  for PR checks, enqueue with `github/pr_merge.py --match-head-commit`, return
  the checkout to the clean default branch and report one line: PR, head SHA,
  `enqueued|pushed`. The worker may hand off at push or at enqueue.
- **Chaperone:** no checkout, no build, cheapest capable model. Handed a PR
  at push (or after a NEEDS-FIXUP push), it first waits for the PR's required
  checks (`pr_merge_watch.py --no-merge-wait`), then enqueues with
  `pr_merge.py --match-head-commit <head>` (or merges by repository policy
  where there is no queue). Handed an already-enqueued PR it skips that step.
  Then one deadline-bounded `pr_merge_watch.py --until merged` call (exit 0 merged, 13
  ejected, 14 unmergeable, 4 timeout). It self-serves once each: `gh run rerun
  --failed` for infra or flakes, a rerun of a stale aggregate from a superseded
  run, a re-enqueue after a batch-mate ejection. Code failures go back as
  `NEEDS-FIXUP <PR> <job> <first error line>`. Maximum 2 automatic retries and
  a 90 minute cap, then `STUCK <PR> <reason>`. Success reports `MERGED <PR> <sha>`.
- **Orchestrator:** counts work done only on `MERGED`. On `NEEDS-FIXUP` the
  worker checks the branch out again, fixes and pushes, then hands it back to a
  chaperone (which waits for checks and enqueues again); these queue ahead of new work and run as an ordinary worker once
  the checkout is free. On `STUCK` it reports what remains. Review may run in
  parallel on the PR diff.

macOS ARM is part of full coverage. `soldr prepare --target aarch64-apple-darwin`
provisions the target-shaped Apple SDK on the Linux builder, so the old
`MACOS_SDK_URL` gate and native macOS fallback no longer exist. The macOS
runners only execute the resulting product bundle in test jobs; Dylint checks
macOS code from Linux by cross-target check, never on a macOS runner.

Two trigger-level notes:

- `pull_request` subscribes to both `labeled` and `unlabeled` so a changed
  selection reruns against the current PR SHA.
- Push coverage narrows from "every branch" to `main`. Branches with no open PR
  no longer get CI. That was a large share of the duplicated fan-out, but it is
  a behaviour change worth knowing about.

## Cross-compilation, per triple, honestly

The workspace's cross-compile surface is small. Two crates compile native code
(`Cargo.lock`): `ring` and `blake3` (both `cc`, routine to cross).
`crates/clud-bin/build.rs` is pure Rust (`protox` + `prost-build`, no `protoc`
binary), so it is not a factor. `whisper-rs-sys` (`bindgen` + a full CMake
project) used to be the third and by far the most cross-compile-hostile —
but `whisper-rs` was removed entirely (voice transcription is stubbed; see
`crates/clud-bin/src/voice/README.md`) after its vendored CMake build
repeatedly broke Windows host builds.

| Triple | Strategy | Notes |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | **native** | the build host; also the clippy/dylint host |
| `aarch64-pc-windows-msvc` | **`soldr build`** | `soldr prepare` provisions the catalogued ARM64 MSVC CRT/SDK and the clang shim that `ring` requires. |
| `x86_64-pc-windows-msvc` | **`soldr build`** | The blessed soldr path provisions the MSVC CRT/SDK and LLVM toolchain. |
| `aarch64-apple-darwin`, `x86_64-apple-darwin` | **`soldr build` + target-shaped Apple SDK** | `soldr prepare` fetches the matching SDK and exports `SDKROOT`; there is no repo secret/variable and no native-builder fallback. |
| `aarch64-unknown-linux-gnu` | **`cargo-zigbuild`** | cleanest cross. |

### Invariant: soldr owns Apple and Windows-MSVC cross builds

**No workflow, build helper or release script may install or invoke a cross
compiler directly for a `*-apple-darwin` or `*-pc-windows-msvc` target.** That
means no `cargo xwin`, no `cargo-xwin`, no `cargo zigbuild` / `cargo-zigbuild`,
no `maturin --zig`, no `zig cc`, no `cross`, no `osxcross`, and no hand-rolled
install of any of them. `soldr prepare --target <triple>` provisions the
toolchain and `soldr build --target <triple>` links against it. Nothing else.

**Zig stays correct for Linux.** `aarch64-unknown-linux-gnu` crosses through
`cargo-zigbuild`, and the manylinux wheel links through `maturin --zig`. The
rule is target-aware, not a blanket ban — a rule that broke the Linux lanes
would be reverted within a day.

`cargo xwin` and `cargo zigbuild --target *-apple-darwin` remain *technically*
reachable, and soldr's own `docs/CROSS_COMPILE.md` documents them as legacy
passthroughs. That is the whole reason this is written down: the fast path and
the slow path are one word apart in a YAML file.

Three things enforce it, because no one of them is sufficient:

| Guard | Catches | Blind to |
| --- | --- | --- |
| `ci/banned_cross_tools.py` (runs in `bash lint` and CI's `Static checks` job) | a literal command in YAML, Python, shell, PowerShell, TOML, Rust or a Dockerfile — including the argv-list form `["cargo", "xwin", ...]` and multi-line install steps | a *conditional* tool at a target held in a variable |
| `ci/xbuild.py::cargo_argv` raises on `zigbuild` + an Apple/MSVC triple | the dispatch itself, whatever the target's provenance | a caller that bypasses `cargo_argv` |
| `tests/test_ci_matrix.py` | every matrix triple's `strategy`, and the argv `cargo_argv` actually returns for it | a command path outside the matrix |

The linter's failure names the file, line, rejected tool, the target family, and
the `soldr build --target ...` replacement — a reader who has never seen this
rule should not have to go looking.

##### Two rule classes (#714)

The linter's original form required a **literal** Apple/MSVC triple on the same
line as the tool. That is right for Zig and wrong for everything else, so the
table is split:

- **Unconditional** — `cargo xwin`, the bare `xwin` CLI, `XWIN_*` /
  `CARGO_XWIN_*` env vars, `osxcross`, `cross` (build/test/run/check/rustc/
  bench/clippy), `Cross.toml`. These name an MSVC- or Apple-only toolchain by
  construction, so there is no target that makes them legal here.
  `cargo xwin build --target $TARGET` and `cargo xwin build --release` both
  fail, where before neither did. This is also the throughput fix: `cargo xwin`
  re-downloads and splats the MSVC CRT/SDK on every cold cache.

  Two shapes worth knowing about, because getting them wrong is how this rule
  goes quietly useless in one direction and gets reverted in the other. The
  `xwin` CLI pattern does **not** require the subcommand to be adjacent to the
  binary — `xwin --accept-license splat --output ...` is the form xwin's own
  README uses, and an adjacency rule would be green in the fixtures and blind
  in production. Conversely `cross` **is** anchored at a command position
  (start of line, a shell/Dockerfile continuation, a YAML `run:`, or a quoted
  argv list), because it is an ordinary English word: unanchored, `name: cross
  build matrix` and `let msg = "cross build failed";` both fail the lint.
- **Conditional on a soldr-owned target** — `cargo zigbuild`, `maturin --zig`,
  `zig cc`. Correct for `*-unknown-linux-*`, rejected at Apple/MSVC.
- **Installs**, at any target, matched against the whole file rather than line
  by line so the GitHub Actions shape (`taiki-e/install-action` with
  `tool: cargo-xwin` two lines below) is caught. Also `cargo binstall`, `brew`,
  `apt`/`dnf`/`apk`/`choco`, `pip install ziglang`, `houseabsolute/actions-rust-cross`,
  `cross-rs/cross`, `tpoechtrager/osxcross`, and a `linker =` override under a
  `[target.<apple-or-msvc-triple>]` section of a Cargo config (quoted or
  unquoted key; `linker` need not be the section's first entry).

An install **suppresses the invocation rules across every line its match
spans**. `cargo install cargo-xwin` is an install, and is also — to the
invocation rules — a mention of `cargo xwin`; reporting both counts one mistake
twice and makes the fix look bigger than it is. Spanning matters: the
`taiki-e/install-action` shape puts `uses:` and `tool: cargo-xwin` on different
lines, so suppressing only the line the match *starts* on would leave the
`tool:` line to be reported a second time. For spanning to be safe the install
match must not reach past the end of its own YAML step, so the window stops at
the next `- ` sequence item: otherwise an install-action for an unrelated tool
searches forward into the *following* step for its `cargo-xwin`, and every
genuine violation in between is silenced on the way. With that bound, a
violation elsewhere in the file is still reported.

One further trap, since it took down the whole linter rather than one rule:
line splitting must use `split("\n")`, never `splitlines()`. `splitlines()`
also breaks on form feed, vertical tab, a lone CR and U+0085/U+2028, none of
which the comment scanner preserves — so a form feed inside a Rust comment (an
Emacs page break) made the original and stripped line lists differ in length,
and `bash lint` died with a `ValueError` traceback instead of printing a
finding.

**Scope.** `.github/`, `ci/`, `bench/`, `crates/`, `dylints/`,
`testbins/`, `tests/`, `.claude/hooks/`, plus the root entrypoints `build lint
test install install.sh install.ps1 publish` and `.cargo/config.toml` — 302
files as of this writing. `crates/` covers the product source and not just the
asset scripts under it: clud shells out to build commands, so a `cargo xwin` in
Rust is a real vector. `vendor/` stays out — third-party source we do not
author. It held `whisper-rs-sys`, whose `build.rs` legitimately reasoned about
zig's C++ runtime for the Linux lanes; that tree is gone, but the exclusion
stays so a future vendored dependency is not linted for invariants that are
ours. `.claude/hooks` rather than `.claude` because the
latter also holds `worktrees/`, an ignored second checkout.

**Escape hatch.** Comments are stripped so prose explaining the ban stays legal.
For `#` languages that is a split; Rust gets a real character scanner, because
every cheap regex is wrong somewhere that matters: Rust block comments **nest**,
a `//` inside a URL must survive (a link to `cargo-xwin` is a real reference and
should be reported), and a stripper with no string awareness turns `let open =
"/*";` … `let close = "*/";` into a one-line bypass for everything between them.
It also understands char literals (`let q = '"';` must not flip its phase, while
`&'a str` is a lifetime, not a delimiter) and **raw strings** — `r"\\?\"` has no
escapes, so a scanner honouring that backslash reads past the closing quote and
treats the rest of the file as string, hiding every violation after it. Five
such literals live under `hook_health/`, and they desynced the scanner for two
review rounds without any fixture noticing; the invariant is now asserted over
every Rust file the linter walks, not just over fixtures. The scanner blanks
comment characters in place, so offsets and line numbers are unchanged by
construction. A module docstring is prose no stripper sees, so a
line carrying `cross-lint: allow` is skipped outright — read from the *original*
line, so a trailing `// cross-lint: allow` in Rust is not itself blanked before
it can be seen. It is
verbose on purpose: `rg 'cross-lint: allow'` lists every escape in the tree
(there are two, both in `tests/test_ci_matrix.py`, where the tool names *are*
the assertion's data). The marker is **strictly line-scoped**, including inside
a multi-line docstring — it must sit on the same line as the tool name, not on
the docstring's closing line.

#### Timing evidence

The blessed path is the one that has run in CI since the matrix moved to
`strategy = soldr`, and the superseded direct-wrapper path is no longer
reachable — deliberately, since making it reachable is the thing this section
forbids. So there is no honest A/B measurement to record here, and a synthetic
one would mean re-introducing the very command path the lint rejects. What can
be said from the workflow runs: the crossed Apple lanes complete in ~5 minutes
and the MSVC lane in ~20–26, all on `ubuntu-24.04`, with native runners doing
no compilation at all. Anyone wanting a genuine comparison should take it from
soldr's own benchmarks rather than from a temporary regression here.

### macOS: SDK provisioning

Linking a darwin binary needs a real macOS SDK for the system frameworks
`cpal`/`rodio`/`arboard` pull in. soldr's blessed Apple-target path resolves the
target-shaped SDK from its toolchain catalogue: the setup composite runs `soldr
prepare --target <apple-triple>` and exports `SDKROOT` plus the target-scoped
compiler and linker settings to later steps.

`ci/xbuild.py` adds nothing for darwin (#648): it used to forward `SDKROOT` as
`CMAKE_OSX_SYSROOT` and turn off ggml's BLAS/Metal backends for whisper.cpp,
which left with whisper-rs (#1207). `build` runs as `soldr build --target`, and
clippy, test harnesses and doc tests run as setup-soldr's target hook `soldr cargo
<verb> --target`. Both Darwin triples build on `ubuntu-24.04`; the native macOS
runners only download and execute the bundles. The Linux-only Dylint job is
separate.

## Cache model

The old design's caches were fragmented by *job type*; the new one is fragmented
only by *target triple*, which is the minimum possible:

- `setup-soldr` with `cache-key-suffix: build-<triple>`. Six namespaces total,
  down from twelve, and each is now written by exactly one job per run rather
  than raced by three.
- The native lane runs `soldr-cook` with flags matching the requested profile.
  Cross lanes skip the cook because setup happens before their target SDK is
  prepared; a host cook cannot be reused by a foreign target. Their per-triple
  build caches still persist the real target artifacts.
- All six build jobs run the same runner image (`ubuntu-24.04`), so host-side
  artifacts — proc-macro crates, build scripts, `protox`/`prost-build` — have
  identical fingerprints across targets. They are still stored per-triple, but
  they compile against a warm toolchain and identical glibc.
- The venv cache key drops its `runs-on` component for build jobs (one OS) and
  keeps it for exec jobs (six OSes).
- Full runs refresh every triple's cache; ordinary `main` pushes refresh only
  Linux x64. PR jobs restore compatible entries via `restore-keys`.

## Bundles: what crosses the wire

`build` uploads one artifact per triple, `bundle-<triple>`:

```
bundle/
  manifest.json         # triple, profile, git sha, test-binary list
  bin/                  # clud, clud-ctrlc-probe, mock-agent, probe-*, scan_zombies
                        # (`clud-cmd-scan`, `clud-shim` are argv[0] aliases of
                        # `clud`, linked on the exec side by ci/aliases.py)
  tests/                # every `cargo test --no-run` harness binary
  dist/                 # the dev wheel (test_trampoline.py needs it)
```

`tests/` is populated from `cargo test --workspace --no-run --message-format=json`,
filtering `reason == "compiler-artifact"` entries that carry an `executable`.

The exec job reconstructs the layout the test code expects rather than requiring
the test code to learn a new one:

- `CLUD_TEST_BINARY`, `CLUD_TEST_BLOCK_BAD_CMD_BINARY`,
  `CLUD_TEST_MOCK_AGENT_BINARY` → `bundle/bin/*`. Every Python test honours
  these first (`tests/test_hello.py:58-60`, `tests/integration/conftest.py:181-200`,
  `tests/test_hook_stdin.py:36-48`), so no `cargo` fallback fires.
- `CARGO_TARGET_DIR` → a synthesized `bundle/target/debug/` containing the
  binaries. `crates/clud-bin/tests/integration/common/mod.rs:33` reads `CARGO_TARGET_DIR` at
  *runtime*, so `mock_agent_path()` resolves for `pty_pump.rs` (13 tests),
  `pty_behavior.rs` (6) and `orphan_reap.rs` (1) with **no source change**, and
  the zombie-scan autouse fixture (`tests/integration/conftest.py:326-355`) stops
  silently degrading to a no-op.

### The one required source change

`env!("CARGO_BIN_EXE_*")` bakes the **builder's absolute path** into the test
binary at compile time, with no runtime override. That breaks 13 tests when the
binary is executed on a different machine:

- `crates/clud-bin/tests/integration/diagnostics/symbols.rs:35` (4 tests)
- `crates/clud-bin/tests/integration/diagnostics/telemetry_endpoint.rs:33` (4 tests)
- `crates/clud-bin/tests/integration/signals/platform/unix/ctrlc_signal_kinds.rs:15` (4 tests, unix)
- `crates/clud-bin/tests/integration/signals/platform/windows/ctrlc_windows_events.rs:28` (1 test, windows)

Fix: a shared `common::bin_path("clud")` helper that prefers a runtime
`CLUD_TEST_BIN_DIR` env var and falls back to the `env!` constant, so local
`cargo test` is unchanged. This is the only production/test source edit the
redesign requires; everything else is CI plumbing.

## Release profile containment

Installer acceptance on a PR runs only while it has the `ci-full` label
(legacy `ci:full` is equivalent); routine PR commits start no installer jobs.
Adding or removing the label re-evaluates the gate, and subsequent commits on
a labeled PR rerun it. The release workflow call remains independent of PR
labels. Labeled PR acceptance builds six direct `clud` candidates from the
exact PR head with `soldr` in the dev profile. `_build-target.yml` uploads one
binary and a provenance record for each target. `ci/installer_candidate.py` checks
source SHA, target format, size, and digest before assembling the six-asset
candidate catalog. Each native Windows, macOS, and Linux runner downloads its
own artifact, installs it, and reports the installed digest and fresh
name-based lookup. The always-run aggregate checks all six host records
against the uploaded binary provenance and fails on a skipped or missing
lane. A fixture-only Cargo feature supplies a local copy of the candidate
catalog and asset bytes through the normal parser and transaction verifier;
release and default builds omit that feature. The fixture includes the
published 2.8.13 wheel row to exercise older-version selection. Both PR and
release acceptance now exercise the native installer.

The PR gate also runs the exact uploaded x64 musl candidate in clean Arch,
Fedora, and Alpine containers. Arch uses fish as the user's default shell;
each distribution creates a fresh nonroot user and proves name-based lookup,
version, and digest. Pinned NixOS 25.05 VM fixtures run on native x64 and
ARM64 Ubuntu runners. They disable both the default stub ELF loader and
`nix-ld`, assert no conventional GNU loader or ELF interpreter, and prove a
fresh nonroot install plus rejection of the historical GNU-only release.
Both matrices are hard dependencies of the always-run aggregate. The ARM64
guest script completed in about 270 seconds on the hosted runner in the first
green run; the job allows 30 minutes including Nix setup and closure download.
The public prerelease and stable workflow calls run matching six-host, three
distribution, and two NixOS lanes from anonymous HTTPS URLs. The NixOS guests
fetch both catalog and binary inside the VM. A separate aggregate rejects a
missing host record, changed digest, failed matrix, or skipped lane. The PR
fixture remains local to the candidate workflow.

`auto-release.yml` snapshots the previous latest release and canonical Pages
digest, publishes final assets as a public prerelease, and attaches a
versioned candidate catalog. Its direct reusable workflow call tests public
candidate bytes before PyPI or stable promotion. Promotion checks unchanged
GitHub asset IDs, then Pages deployment and a second direct reusable call
test the canonical `latest-stable` path. If the post-promotion path fails,
the rollback job demotes the release and redeploys the previous pointer.
The release and both Pages workflows share a serialized queue; each release
rechecks public state before mutation.
Reusable installer jobs select release mode from the required `release_tag`
input. A called workflow inherits the tag push as `github.event_name`, so an
event-name check for `workflow_call` would skip every public lane. Explicit
always-run caller gates reject a skipped candidate or released matrix; the
released gate stays red even when rollback restores the previous pointer.
After the first native candidate and stable public gates passed, APE build,
publication, and acceptance jobs were retired. Existing release assets remain
available from their historical tags.

Native Linux downloads use two additional release-profile jobs, one for each
`*-unknown-linux-musl` architecture. Both build on x64 Ubuntu because soldr's
musl compiler bundles are x64-hosted. The ARM artifact is then downloaded and
executed on a native ARM Ubuntu runner before either publication path starts.
The jobs use `soldr prepare` and `soldr build`, and stage only a `clud` ELF in
`standalone/`. The existing six wheel jobs still produce all PyPI
wheels and the sdist. `installer/release_assets.py` extracts the four Windows
and macOS direct downloads from those wheels; it never relabels a GNU Linux
wheel executable as musl. `installer.verify_native_assets` checks the exact
six-file direct-download set before checksums and release upload. The musl
assets must have the expected ELF machine and no `PT_INTERP` or `DT_NEEDED`.

The static musl build artifacts use `standalone-musl-*`, outside the `wheels-*`
pattern consumed by PyPI. The release publisher downloads them from the same
candidate SHA as the wheel jobs. Host and NixOS installation checks must use
the uploaded bytes; see [installer Pages](installer-pages.md) for catalog
compatibility rules.

Requirement: nothing builds `--release` except the release pipeline.

- `_build-target.yml` takes `profile` (`dev` | `release`), defaulting to `dev`.
- `ci.yml` never passes `profile`, and has no input that could set it.
- `_build-target.yml` opens with a guard step that fails when
  `profile == 'release'` and `github.workflow != 'Auto Release'`. A reusable
  workflow sees the *calling* workflow's name, so this is enforceable in YAML
  and cannot be bypassed by a `workflow_dispatch` on the template.
- The 24 deleted leaf workflows each carried a `build-mode: [dev, release]`
  dispatch choice — six user-reachable paths to a release build outside the
  release pipeline. Deleting them closes that surface.
- `--zig --compatibility manylinux2014` (`ci/build_wheel.py:48-51`) stays on the
  release path only; CI dev wheels are plain `--profile dev`.

### Debug info goes to a sidecar, not into the wheel

PyPI caps a project's individual files at 100 MB. `[profile.release]` set
`debug = "line-tables-only"` with no `split-debuginfo`, and on ELF that DWARF is
embedded in the binary while Windows/macOS write it to a `.pdb` / `.dSYM` the
wheel never sees. That asymmetry made the manylinux wheel ~7x the others; it
crossed 100 MB at 2.7.2 and killed the PyPI upload for 2.7.2, 2.7.3 and 2.7.4.
Because `publish-pypi` failed, the dependent `publish-release` job was skipped,
so those tags produced no GitHub release either — the pipeline was silently
broken for three tags.

`split-debuginfo = "packed"` produces the `.dwp` sidecar, but Cargo/toolchain
combinations can still leave `.debug_*` sections in the linked executable.
`strip = "debuginfo"` is therefore the release-wheel boundary: it removes only
debug sections after the sidecar exists, while retaining the ordinary symbol
table used by runtime diagnostics. The release x86_64 Linux wheel is capped at
20 MB, so a future regression fails before PyPI upload.

Only Linux changes. `packed` is already MSVC's default. On Apple it would select
the `.dSYM` bundle, which means rustc runs `dsymutil` at link time -- the one
way this setting could plausibly break a cross lane. It cannot: `soldr prepare`
probes for dsymutil and, when it is missing, exports `-Csplit-debuginfo=off` in
both `CARGO_TARGET_<T>_RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` for Apple
triples, which outranks the profile key. Verified by inspecting `soldr prepare
--github-env` output per triple: the override is emitted for
`aarch64-apple-darwin` and for neither `x86_64-unknown-linux-gnu` nor
`x86_64-pc-windows-msvc`. That asymmetry is load-bearing -- if soldr ever
emitted it for linux-gnu, this fix would silently stop working and the wheel
would grow back.

The `.dwp` is attached to the GitHub release, and the routing is the part to not
break:

- `ci/xbuild.py::collect_debuginfo` stages it under `dist-debuginfo/`,
  **never** `dist/`. `_build-target.yml` uploads `dist/*` as the `wheels-*`
  artifact and `publish-pypi` hands that to twine as `packages-dir`; a
  non-package file there is the exact 400 this change removes.
- It ships as its own `debuginfo-<triple>` artifact, which `wheels-*` cannot
  match. Only `publish-release` downloads that pattern.
- Every hop is non-fatal (`if-no-files-found: ignore`, `continue-on-error`,
  `fail_on_unmatched_files: false`), because targets where `packed` writes no
  `.dwp` must not block a release. The guarantee `fail_on_unmatched_files` used
  to give is asserted explicitly instead: the checksums step runs
  `ls dist/*.whl`.
- Beside each `.dwp`, `ci/build_ids.py` stages a `clud-<triple>.build-id`
  fragment read from the shipped wheel's `clud`. `publish-release` folds them
  into a `BUILD-IDS.txt` asset before the checksums step, so `SHA256SUMS`
  covers it; `clud symbols install` pairs a report to a sidecar through it
  ([crash-reports.md](crash-reports.md#sidecar-pairing-by-published-build-id-1016)).
  No PR workflow runs `publish-release`, so this path is exercised only by a
  release.

Only `clud` itself gets a `.dwp`. It is the sole binary that installs the crash
reporter, and the shims currently share its whole dep tree, so publishing all
five would put ~900 MB of near-duplicate DWARF on every release page.

Maturin's binary binding includes every enabled Cargo `[[bin]]`. The wheel
packer removes `clud-ctrlc-probe` after packaging because it is a real-signal
test fixture, not a production command; CI bundles retain it for those tests.

### The manylinux glibc floor is `--compatibility`, not `--target`

The release branch of `ci/xbuild.py::cmd_wheel` owns this and is the only place
that should.

The platform tag selects the floor: `--compatibility manylinux2014` asks
maturin to audit for glibc 2.17. `--target` remains the ordinary Rust triple;
it must not carry a synthetic `.2.17` suffix. `soldr prepare` supplies the
matching catalogue GNU sysroot and linker to every Linux release lane, so no
zig toolchain or environment scrubbing is involved.

None of this is exercised by `ci.yml` — `_build-target.yml` refuses
`profile: release` outside Auto Release — so the release wheel path is only
ever proven by a real tag. Treat the release-wheel unit tests in
`tests/test_ci_xbuild.py` — `test_release_linux_wheel_builds_without_zig` and
`test_release_linux_wheel_env_sets_no_whisper_vars` — as the standing contract.

#### The static C++ runtime link is gone (#1207)

soldr's catalogue GNU toolchain pins *glibc* at 2.17 through its sysroot but
does not pin the *C++* runtime, so while whisper.cpp was in the graph the
release wheel appended `-static-libstdc++`/`-static-libgcc` to soldr's
exported `CARGO_ENCODED_RUSTFLAGS` and set `WHISPER_LINK_CXX_STATIC=1` — the
load-bearing half, because whisper-rs-sys emitted an explicit
`cargo:rustc-link-lib=dylib=stdc++` that a driver flag for the *implicit*
libstdc++ cannot override. whisper-rs was removed (and `vendor/` with it), and
`Cargo.lock`'s only native dependencies are now `ring` and `blake3`, both C.
Nothing links libstdc++, so the whole mechanism was inert and is deleted.

What remains dynamic is Rust's own unwinder importing `libgcc_s.so.1`, which
the manylinux_2_17 policy whitelists at the `GCC_3.x`/`GCC_4.2.0` symbol
versions Rust references. Nothing here is taken on faith, and nothing here is
provable by unit test either: the release wheel build audits itself
(`--compatibility manylinux2014`), so a green

```
python -m ci.xbuild wheel --target x86_64-unknown-linux-gnu --strategy soldr --profile release
```

is the gate — for #1207's removal of `-static-libgcc` and for any later change
to these flags. `_build-target.yml` refuses `profile: release` outside Auto
Release, so CI will not run it for you; run it before merging such a change.
If the audit ever rejects a too-new `libgcc_s` import, restore `-static-libgcc`
alone — never `WHISPER_LINK_CXX_STATIC`, which has no build script left to
talk to — and amend this subsection.

### Reproducing a wheel locally

The same driver builds a wheel on a developer machine (#1017):

```
python -m ci.xbuild wheel --target x86_64-unknown-linux-gnu --strategy soldr --profile release
```

Module form only: `python ci/xbuild.py` is rejected at entry, because running
the file by path shadows `ci/` with any `ci` distribution in site-packages.

CI persists the target toolchain env with `soldr prepare --target <t>
--github-env "$GITHUB_ENV"` in `setup-build`, a mechanism with no local
counterpart. So outside GitHub Actions, `ci/xbuild.py::prepare_toolchain_locally`
runs that same `soldr prepare` into a temporary file and applies every exported
variable in-process before dispatching, which every cargo/soldr/maturin child
then inherits. It is inert under `GITHUB_ACTIONS=true`, skippable with
`CLUD_XBUILD_SKIP_PREPARE=1`, and silently absent when no `soldr` is on PATH
or in the repo `.venv` —
in which case `cross_toolchain_preflight` names the missing cross compiler at
entry rather than letting `ring`'s build script discover it minutes later.
Wheel sizes are then checked by `python -m ci.check_wheel_size --dist-dir dist/`.

### No empty test harnesses (#1714)

`cargo test --workspace --no-run` builds one harness per target whose `test`
setting is on, and each harness statically links the whole workspace. zccache
never caches harness link products (zackees/zccache#1525), so an empty one
costs its full compile and link on every run and ships in every bundle. A
target with no `#[test]` in its module tree (the `clud` bin, the probe and
stub testbins, `clud-ctrlc-probe`) declares `test = false`;
`ci/banned_empty_harnesses.py` fails `bash lint` otherwise. Bins stay built
for `CARGO_BIN_EXE_*` and the bundle: `test = false` only drops the harness.

### Two harnesses in the `clud` package (#1726)

Removing the empty harnesses did not shorten the harness step: the cost was
the harnesses that link the whole workspace. The `clud` package now builds
two, its lib unit tests and one `integration` target
(`crates/clud-bin/tests/integration/main.rs`, one module per category), down
from eight (the lib, six category targets and `clud-kittyterm-paste`, whose
logic and tests moved into `clud::paste_image`). `ci/harness_budget.py`
fails `bash lint` on a third. `ci/run_bundle.py` keeps the old isolation: each
category of `integration` runs in its own process, selected by exact test
name (`ci/harness_plan.py`), and each `pty::` test in its own
pseudo-terminal. The remaining floor is the lib's unit-test harness, a full
compile of the 212K-line crate in test mode (about 40 s alone; type check and
borrow check are single-threaded).

When `cargo test --no-run` fails, `ci/xbuild.py` prints the rendered compiler
errors from its JSON output and the captured stderr, newline-terminated.

### Wheel script modes and the release-wheel smoke (#1545)

pip installs each `.data/scripts/*` entry with the Unix mode in its zip
`external_attr`; 2.8.14 and 2.8.20 shipped them 0644 (#1544). Three guards:

- **One writer.** `ci/wheel_rewrite.py` (`rewrite_wheel` / `write_wheel`) is
  the only code in `ci/` that writes a zip. Kept entries reuse their original
  ZipInfo; added entries state a mode (`.data/scripts/*` defaults to 0755);
  RECORD is regenerated once. prune, the release ELF strip, the windows-gnu
  repair, the Kitty bundle and the webterm companion all go through it.
  `ci/banned_wheel_writes.py` fails `bash lint` on `extractall(` or a
  write-mode `ZipFile` anywhere else in `ci/`.
- **Static gate.** `python -m ci.check_wheel_modes --dist-dir dist/` (stdlib
  only, copyable to other maturin repos) fails a non-Windows wheel whose
  scripts are not executable regular files. `_build-target.yml` runs it after
  "Check wheel size" for bundle and release builds; `xbuild wheel` runs it too.
- **Release install smoke.** `auto-release.yml`'s `wheel-smoke` job calls
  `_run-tests.yml` with `suite: wheel-smoke` on each native Linux/macOS exec
  runner (`ci_matrix.wheel_smoke_matrix`). `ci/wheel_smoke.py` pip-installs
  the exact release wheel into a fresh venv and runs
  `verify_installed_scripts` (exec bits via `os.access`, `clud --version`, hook
  smokes). Both publish jobs `need` it.

## Deduplicated checks

| Check | Before | After |
| --- | --- | --- |
| `ruff` | 6x | 1x (`static`) |
| `cargo fmt --check` | 6x | 1x (`static`) |
| `ci/banned_imports.py` | 6x | 1x (`static`) |
| `ci/banned_cross_tools.py` | — (#637; new) | 1x (`static`) |
| `cargo clippy --workspace --all-targets` | 6x native | 2x, both on Linux |
| dylint | 2x per PR (`push` + `pull_request` both fire) | 1 Linux job per PR: host + Windows/macOS cross-target passes |
| Rust doc-tests | 6x | 1x (host triple) |

`ci/lint.py` has `--static-only`, and the checks inside it are ordered
cheapest-first (ruff → banned imports → banned cross tools → `cargo fmt`) so
the most common failure
reds out in seconds instead of behind a cargo subprocess. `bash lint` with no
flags runs the whole suite including host Dylint after Clippy.

**Clippy runs on two triples, not six.** It is worth stating why, because the
obvious intuition is wrong: clippy is *not* nearly free once the dependency
graph is warm. `cargo clippy` builds a **Check-mode** unit graph, emitting
`.rmeta` under a different unit hash than the `.rlib` that `cargo build` and
`cargo test --no-run` need. Cargo's unit mode is part of the fingerprint, so
there is no reuse in either direction and reordering the steps does not help —
clippy is a second full pass over all ~429 dependencies. Since the platform
gating in this workspace is by OS rather than architecture, `x86_64-unknown-linux-gnu`
plus `x86_64-pc-windows-msvc` type-check every `cfg(windows)` / `cfg(unix)`
branch. The other four triples would pay a full extra pass for no new coverage.

**Dylint is required in every mode.** `_dylint.yml` is one `ubuntu-24.04` job,
not a runner matrix. A late lint only observes code that compiles for the
checked target, so the host pass never reaches `cfg(windows)` or
`cfg(target_os = "macos")` modules. The job therefore adds check-only passes
with `--target x86_64-pc-windows-msvc` and `--target aarch64-apple-darwin`.
`soldr dylint prepare --target` installs the pinned nightly's `rust-std` for
each target; Soldr's verified 6.2.0 tools and nightly driver stay prebuilt, so
no pass builds a toolchain. Dylint lints only workspace crates, but each pass
must first type-check every dependency with that nightly, which the stable
build caches cannot supply. zccache caches those check units per target in the
`dylint-zccache-<pass>` build-cache entry per matrix leg, saved from `main` only: measured warm, the
host/Windows/macOS passes took 28s/49s/69s at a 100% hit rate, against
2m45s/2m41s/1m21s with `ZCCACHE_DISABLE` set. The rest of a warm cross pass
downloads that target's `rust-std` and cross sysroot (LLVM, MSVC or Apple SDK)
each run. One triple per OS is enough for the same reason
Clippy needs only two: this workspace gates by OS, not architecture. The
`CI OK` gate requires the Dylint result even on an unlabeled PR.
The passes are parallel legs of one Linux matrix (#2061): back to back they
took 5m20s and were the slowest required job once the build cook went warm.

**Doc-tests run once.** They were covered by the old `cargo test --workspace`
but produce no harness binary, so they cannot ride along in a bundle. They are
OS- and architecture-independent, so one run on the host triple is full
coverage rather than a reduction. That run is in the Linux x64 Clippy job
(`doctest: true`), not the build job: every unit lane waits for the build job
to complete, and the Clippy job finishes over a minute earlier.

## Template inventory

Deliberately small, to keep the ~60 lines of soldr/uv boilerplate that is
currently copy-pasted four times in exactly one place.

| File | Role |
| --- | --- |
| `.github/actions/setup-build/action.yml` | composite: python + uv + venv cache + `setup-soldr` + `uv sync`. Used only by build-side jobs. |
| `.github/actions/setup-exec/action.yml` | composite: python + uv + `uv sync --group test`, then **deletes** the Rust toolchain. Used by exec jobs. |
| `.github/workflows/_build-target.yml` | reusable: one triple → one bundle (+ optional wheel/sdist artifact). Called by `ci.yml` and `auto-release.yml`. |
| `.github/workflows/_run-tests.yml` | reusable: one triple × one suite → test execution. |
| `.github/workflows/_dylint.yml` | reusable + dispatchable: one Linux Dylint job (host + Windows/macOS cross-target). |
| `.github/workflows/ci.yml` | the only push/PR entrypoint. |
| `.github/workflows/auto-release.yml` | unchanged triggers; now the sole caller that may pass `profile: release`. |
| `ci/ci_matrix.py` | the triple table, shared by CI and the release matrix. |
| `ci/xbuild.py` | every cargo/maturin invocation + the per-strategy cross environment. |
| `ci/bundle.py`, `ci/run_bundle.py` | pack the bundle / execute it on the exec runner. |

Deleted: 24 `{linux,macos,windows}-{x86,arm}-{build,lint,unit-test,integration-test}.yml`,
plus `_lint.yml`, `_unit-test.yml`, `_integration-test.yml`, `_build.yml`,
`dylint.yml`.

### When a job hits the 20-minute ceiling

`_run-tests.yml` caps every exec job at 20 minutes, on the assumption that
pytest-timeout (`timeout = 90`, thread method, plus `faulthandler_timeout`)
names a hung test long before that. #1168 showed the gap: a wedged Windows
integration job was cancelled at the ceiling and GitHub kept **no log at all**
for the cancelled step, so neither the `-v` progress nor any stack dump
survived. Two things close it:

- `ci/run_bundle.py::run_streamed` runs pytest through running-process,
  echoes each line and tees it to `logs/pytest-<suite>.log`, flushed per
  line, with `PYTHONUNBUFFERED=1` on the child.
- `ci/pytest_progress.py` also flushes test start/finish records to
  `logs/pytest-<suite>-progress.jsonl`. An unmatched start identifies the test
  active when pytest exits before writing a summary or JUnit XML (#1178).
  The plugin reads `CLUD_PYTEST_PROGRESS_LOG` once at import, before
  `tests/conftest.py` scrubs `CLUD_*` from the environment test children
  inherit (#1625).
- The `Upload failure logs` step runs on `failure() || cancelled()`, so that
  file reaches the artifact even when the step log did not.

On the clud side, `CLUD_EXIT_TIMING_FILE` (set by the integration harness on
every launch, see `tests/integration/_daemon_helpers.py::run_clud`) records
`launch-stage` breadcrumbs (`backend_run`, `child_wait`, `child_teardown`,
`runtime_drop`, `cpu_banner_stop`, …) ahead of the `exit-stage` ones from #594, so a process the
harness had to kill names the stage it was in; implementation in
`crates/clud-bin/src/stage_trace.rs`.

### pytest temp retention (#1686)

`tmp_path_retention_policy = "failed"` in `pyproject.toml` removes a passing
test's `tmp_path` at once and a passing run's base temp at session end, so
harness worlds (repo, bare origin, Claude config) no longer pile up under
`$TMPDIR` (`~/.clud/tmp` inside a clud session) until the 72 h sweep.
`ci/pytest_tmp_retention.py`, loaded by `tests/conftest.py`, redoes that
removal with a handler that clears the read-only bit (pytest's own `rmtree`
ignores errors and leaves git object stores behind on Windows) and, on a
failed run, prints the kept base temp's path. An explicit `--basetemp` is
never removed. Covered by `tests/test_pytest_tmp_retention.py`.

### Two traps worth naming

**Never use `uv run` in a workflow step.** `pyproject.toml` sets
`build-backend = "soldr"`, so `uv run` syncs the *project*, which triggers a
full PEP 517 maturin build of the Rust binary before your command starts. On a
build job that is a wasted host-wheel build; on an exec job it hits the removed
toolchain and fails every test. Both composite actions export `$VENV_PY`
pointing at the synced interpreter — use that. The repo's `lint` script
(`lint:8-24`) already worked around this for the same reason.

**Shadowing cargo on PATH is not enough on Windows.** Rust's
`Command::new("cargo")` goes through `CreateProcess`, which only appends
`.exe` — a `cargo.cmd` shim is skipped and the real `cargo.exe` found. Since the
test suite spawns cargo from Rust (`crates/clud-bin/tests/integration/common/mod.rs:78-116`),
`setup-exec` deletes the toolchain binaries outright and then installs failing
shims for the error message. Runner VMs are ephemeral, so this is safe.

## Expected effect

Per PR push, `core` tier:

| | Before | After |
| --- | --- | --- |
| Workflows triggered | 12 | 1 |
| Full workspace compiles | ~12–18 | 3 |
| Clippy passes | 6 | 2 |
| Cache namespaces | 12 | 3 (of 6) |
| macOS runner jobs | 4 cold builds | 2 exec only |
| Windows runner jobs | 4 cold builds | 2 exec only |
| Platform-independent lint runs | 18 | 3 |
| dylint runs | 2 | 1 Linux job (host + 2 cross-target passes) |

Critical path is `build-<triple>` → `test-<triple>` per lane, in parallel across
lanes, with `static` failing fast alongside. The Linux lane reports red/green
roughly 15 minutes before the slowest lane finishes, because no lane waits on
another.

### Known remaining costs

Not fixed here, recorded so they are not rediscovered:

- **Cross-lane cold starts.** The native lane can reuse `soldr-cook`, but cross
  lanes prepare their SDK after setup-soldr and deliberately skip a host-only
  cook. A miss in the per-triple target/build cache still means compiling the
  foreign dependency graph once.
- **The 10 GB per-repo Actions cache quota.** Six per-triple `target/` caches
  plus the dylint cache plus the venv caches plausibly exceed it, and eviction
  is silent — `restore-keys` simply miss and the job rebuilds cold.
  `CARGO_INCREMENTAL=0` (set in `_build-target.yml`) is the cheap mitigation
  already applied; splitting deps from workspace crates in the cache key, and
  sharing the host-side proc-macro/build-script units across all six triples,
  are the next steps.
- **The linux-x86 lane still round-trips through an artifact** even though its
  build and exec runners are the same class. Running its suites inline would
  save ~3–5 minutes at the cost of the uniform template and the structural
  "exec cannot compile" guarantee.
- **Bundle size.** Each harness statically links the whole workspace; per-OS
  filtering is applied in `ci/bundle.py`, but `split-debuginfo = "packed"` plus
  excluding the debug files from the bundle would cut substantially more.

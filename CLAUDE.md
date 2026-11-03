# CLAUDE.md

Guidance for Claude Code when working in this repository.

**This file is an index.** Per-directory `README.md` files carry the real detail — descend into them as needed instead of expanding this file.

## Quick Reference

### Essential Commands

The commands below describe what the CI jobs execute. Run focused checks
locally when useful, then push and require the selected GitHub Actions jobs
to pass before merging. Bosn is optional; no local receipt is required.
Changes to Windows platform implementations or native runtime contracts
require `ci-windows`, including the relevant PTY tests. Shared business logic
calling unchanged Windows APIs stays on routine CI; inspect the changed code
before selecting native coverage.

- **Build**: `bash build` — dev wheel (Rust binary + Python package)
- **Lint**: `bash lint` — `cargo fmt`, `cargo clippy`, `ruff` (**MANDATORY** after any code edit). Clippy runs on the host and every target in `Cargo.toml`'s `[workspace.metadata.soldr].targets` (Windows, both macOS, Linux arm64) through soldr, so platform `cfg` code fails at lint; `bash lint --host-only` skips the cross targets for a fast edit loop ([ci.md](docs/architecture/ci.md#local-validation-before-remote-ci))
- **Test (Tier 0, host)**: `bash test --host` — Rust library tests + Python tests with no built binary or daemon; run constantly ([tiers](docs/architecture/ci.md#two-local-test-tiers-2356))
- **Test**: `bash test` — Rust unit tests + Python unit tests
- **Test (full)**: `bash test --integration` — adds integration tests with mock agents

### Soldr (Rust toolchain wrapper)

All `cargo` / `rustc` / `rustfmt` calls **must go through [soldr](https://github.com/zackees/soldr)**: `soldr cargo build`, `soldr cargo test -p clud-bin`, etc. soldr resolves the rustup-managed toolchain via `rustup which`, sidestepping chocolatey cargo on Windows and other stale PATH shims. A `.claude/hooks/check-soldr.py` PreToolUse hook enforces this.

Agent validation runs should add the bounded mode: `SOLDR_PORCELAIN=1 soldr cargo test ...`
prints ONE JSON line (verdict, `exit_code`, counts, `log_path`) instead of relaying every
passing test and per-crate cache record. Open `log_path` only when `exit_code` is non-zero;
if stdout is not a JSON object the installed soldr predates the mode, so read the output as
usual (forge-ci/soldr#3924, clud#2231).

**Batch validation (clud#2313).** This is about the edit loop's soldr calls; each call
pays its own soldr process start and banner. While iterating, run one `soldr lint`
(fmt check, Clippy, configured Dylint; `soldr lint --host-only` skips cross-target
clippy) instead of separate fmt/clippy calls, and one
`SOLDR_PORCELAIN=1 soldr cargo test -p a -p b` with targeted filters instead of
per-crate calls. `soldr lint` does not replace `bash lint`: that stays the mandatory
gate (repo guards, Ruff, pinned Dylint setup), as do the hosted Static checks, Clippy
and Dylint jobs.

Install soldr: `./install` (puts it in this repo's `.venv`) or `./install --global` (puts it in `~/.cargo/bin` or `~/.local/bin`). CI uses `forge-ci/setup-soldr@v0`.

### Validation before merging

GitHub Actions is the CI of record. Push the change and require `CI OK`
before merging. PRs execute every selected hosted check; neither publication
nor CI requires a Bosn run or a `Local-Gate`/`Ci-Attestation` trailer.

Use targeted local tests and lint for the edit loop. Rust commands still go
through soldr. An optional `bosn ci run --workspace . --trigger pr` or
`clud ci [JOB] [--filter RE]` can replay supported jobs in an isolated engine.
If Bosn is unavailable or busy, use hosted CI. Stop only your own Bosn run;
never kill other sessions' processes. Native Windows/macOS behavior needs the
relevant native CI lane; a local run on another OS cannot prove it.

Agent landing (workers, chaperones, labels): see [`ci.md`](docs/architecture/ci.md#agent-landing-workers-end-at-enqueue-chaperones-own-the-merge).

See [`docs/architecture/ci.md`](docs/architecture/ci.md#local-validation-before-remote-ci)
for optional replay commands and coverage limits. Do not weaken a test or
waive a hosted job to obtain a green result.

Optional replay needs bosn 0.1.10 or newer. Run its full plan at most once per change;
see the [local CI budget](docs/architecture/ci.md#local-ci-budget). Never use
`pkill` or `pgrep -f` against Bosn runs; cancel only your own run.

## Repository Map

This is a Rust CLI (`clud`) distributed as a Python wheel via maturin (`bindings = "bin"`). The Rust source lives under `crates/` and is mirrored by a progressive-disclosure README tree:

```
crates/                    → see crates/README.md
  clud-bin/                → see crates/clud-bin/README.md
    src/                   → see crates/clud-bin/src/README.md
      command/             → see crates/clud-bin/src/command/README.md
      daemon/              → see crates/clud-bin/src/daemon/README.md
      dnd/                 → see crates/clud-bin/src/dnd/README.md
      platform/            → see crates/clud-bin/src/platform/README.md (the only place a host is named)
      self_install/        → see crates/clud-bin/src/self_install/README.md
      test_runtime/        → see crates/clud-bin/src/test_runtime/README.md
      toast/               → see crates/clud-bin/src/toast/README.md
      voice/               → see crates/clud-bin/src/voice/README.md
    tests/                 → see crates/clud-bin/tests/README.md
    assets/                → see crates/clud-bin/assets/README.md
      skills/              → see crates/clud-bin/assets/skills/README.md
        clud-issue/        → see .../clud-issue/README.md
        clud-issue-triage/ → see .../clud-issue-triage/README.md
        clud-tag-release/  → see .../clud-tag-release/README.md
testbins/                  → see testbins/README.md
  mock-agent/              → see testbins/mock-agent/README.md
    src/                   → see testbins/mock-agent/src/README.md
docs/                      → see docs/README.md
  ARCHITECTURE.md          # index of subsystem topic docs
  DESIGN_DECISIONS.md      # ADR-style records (DD-001 … DD-039)
  architecture/            # one file per cross-cutting subsystem
src/clud/__init__.py       # Minimal Python package (version shim only)
ci/                        # CI scripts (env, build, lint, test)
tests/                     # Python tests (unit + integration)
```

### How to navigate

#### Terminal / PTY work

Before changing or debugging anything between the user's terminal and the
child PTY (input path, escape sequences, raw mode, ConPTY, Ctrl+C, terminal
restore), load the platform skill: `terminal-pty-windows`,
`terminal-pty-linux` or `terminal-pty-macos` (`.claude/skills/`). They share
[`docs/architecture/terminal-pty.md`](docs/architecture/terminal-pty.md),
which owns the cross-platform invariants.

#### Performance benchmarks

Standalone, opt-in performance harnesses live in [`bench/README.md`](bench/README.md).
They are not pytest tests; use the idle CPU runbook there when validating an
end-to-end daemon/client performance change.

- **Where is X implemented?** Start at [`crates/clud-bin/src/README.md`](crates/clud-bin/src/README.md). It groups every top-level `.rs` file by concern and includes a "Quick lookup — which file owns a given subcommand" table.
- **What's in this directory?** Each directory's `README.md` lists its files, key public items with `file:line` refs, and who calls into it.
- **How does a subsystem work end-to-end?** [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — topic docs that span multiple directories (loop, daemon IPC, session lifecycle, skill system, gc/registry, Windows quirks, launch plan).
- **Why was it designed this way?** [`docs/DESIGN_DECISIONS.md`](docs/DESIGN_DECISIONS.md) — ADR-style rationale for non-obvious choices.
- **How does a test work?** [`crates/clud-bin/tests/README.md`](crates/clud-bin/tests/README.md) for Rust integration tests; [`testbins/mock-agent/README.md`](testbins/mock-agent/README.md) for the mock backend.

## Architecture & design docs

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — index of subsystem topic docs (each ~150–400 lines, self-contained).
- [`docs/DESIGN_DECISIONS.md`](docs/DESIGN_DECISIONS.md) — 10 ADRs covering the non-obvious choices below and more.

### `grind` directive

[`docs/architecture/grind.md`](docs/architecture/grind.md) is the single
owner of the `clud grind` execution contract and its `/grind` skill DAG.
`grind` opens one normal interactive PTY session seeded with `/grind`; the
bundled workflow and capped `grind-*` agents do the work, and cron mode uses
the harness's native `/loop`. The harness owns repetition and termination.
A new `grind-*` role needs its agent file, a `claude_files.rs` entry, and a
policy in `block_bad_cmd_grind_caps.rs`. Do not implement,
restore, or emulate an external clud prompt/relaunch loop for `grind`: no
DONE/BLOCKED markers, iteration cap, headless `-p`/`exec`, or stream-json
renderer. A harness without native interactive `/loop` support must fail
explicitly; it must not fall back to clud-managed looping. This supersedes the
external-orchestration guidance in issue #897 and PRs #950 and #1045.

## Where to put new docs

Tiered to keep agent context windows small and prevent duplication:

1. **Per-directory README** (`<dir>/README.md`) covers **what's in this directory** — files, key types with `file:line`, callers. If a fact applies only inside one directory, write it here.
2. **Subsystem topic doc** (`docs/architecture/<topic>.md`) covers **how a subsystem works across directories**. If a concept spans 2+ directories or 3+ files, write it here and have the per-dir READMEs link in with a one-line breadcrumb.
3. **Design decision** (`docs/DESIGN_DECISIONS.md`, append-only `DD-NNN`) covers **why** a non-obvious choice was made. If a reader could plausibly ask "why didn't you do it the other way?", add a DD.
4. **Never duplicate.** One doc owns each fact; everyone else links. When you find yourself copying a paragraph, replace the copy with a breadcrumb.

For a new cross-cutting feature: add the topic doc → register it in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) → add a breadcrumb in each touched per-dir README → if the design is non-obvious, append a `DD-NNN` to `DESIGN_DECISIONS.md`.

## Key Design Decisions (summary)

See [`docs/DESIGN_DECISIONS.md`](docs/DESIGN_DECISIONS.md) for full rationale.

- **YOLO by default** — the effective harness's permission-bypass flag is auto-injected unless `--safe` ([DD-002](docs/DESIGN_DECISIONS.md#dd-002-yolo-mode-is-the-default-safe-is-the-opt-out)).
- **Backend agnostic** — supports both `claude` and `codex` via `--claude` / `--codex` ([DD-004](docs/DESIGN_DECISIONS.md#dd-004-backend-agnostic--support-both-claude-and-codex)).
- **Single `LaunchPlan`** — production launches go through `command::build_launch_plan_for_target`; `build_launch_plan` is the native compatibility/test wrapper, and `--dry-run` emits the resolved plan as JSON ([DD-005](docs/DESIGN_DECISIONS.md#dd-005-single-launchplan-as-source-of-truth-for-everything-clud-runs), [launch-plan.md](docs/architecture/launch-plan.md)).
- **Unknown flag passthrough** — unrelated backend flags are forwarded unchanged; top-level flag-shaped near misses are rejected with a clap suggestion, and `--` is the explicit forwarding escape hatch. Exact `-deepseek` and Unicode-dash spellings of public clud options are corrected before parsing.
- **Test-first** — every feature has both Rust `#[test]` and Python subprocess tests.
- **OpenRouter model selection** — OpenRouter is a gateway while Claude Code
  remains the frontend. `clud --openrouter` uses the reviewed Sonnet alias,
  `--model <wire-id>` pins startup, and Claude Code's `/model` shows the live
  gateway-discovered rows **alongside** its own built-in ones — discovery adds
  rows and cannot subtract them, so the picker is not constrainable from clud
  ([DD-054](docs/DESIGN_DECISIONS.md#dd-054-the-model-picker-belongs-to-the-harness-and-discovery-only-adds-rows)).
  Do not add a second clud-side picker or fold changing OpenRouter
  inventory into the static catalog. Preserve independent role-model mappings
  and treat non-Claude models as best-effort; see
  [`provider-selection.md`](docs/architecture/provider-selection.md#openrouter-model-selection-contract).

## Code Quality Standards

For agents, the full PR plan fulfills the lint requirement below. Focused
`--job static-checks` runs formatting/Ruff/guards, `--job lint-linux-x64`
runs Clippy/doc-tests, and `--job dylint` runs Dylint. `--job static` only
resolves CI mode. Local checks are optional; the selected hosted jobs must pass.

After any code edit, the relevant lint checks must pass. The hosted Static
checks, Clippy and Dylint jobs fulfill this requirement. For local feedback,
`bash lint` runs formatting, Clippy, Ruff and the host Dylint pass.

### Function-complexity ratchet

Every function stays under the linters' default ceilings: clippy's
`too_many_lines` (100) and `cognitive_complexity` (25), set in the root
`Cargo.toml`'s `[workspace.lints.clippy]` and inherited by every member, and
ruff's `C901` (McCabe 10). Functions already over a ceiling carry
`#[expect(clippy::..., reason = "complexity ratchet baseline ...")]` or a
`# noqa: C901` on the `def` line; never `allow`, never a file-level waiver.
When a refactor brings one under the ceiling, the unfulfilled `expect` (or
RUF100's unused `noqa`) fails lint, so delete the marker in the same change.
New code gets no marker: split the function instead.
Policy: [zackees/ci.yml#229](https://github.com/zackees/ci.yml/issues/229)
(`RUST-018`, `PY-004`).

### Process execution: `running-process` only

**Python's `subprocess` module is banned everywhere.** Do not import it, invoke it indirectly, or introduce any new exception. Python's blocking stdout/stderr reads have caused serious problems in the toolset.

**Rust's `std::process` APIs are banned for the same reason.** Do not use them to launch or manage processes. Use the `running-process` bindings exclusively in both languages: `running-process` provides efficient, non-blocking streamed stdout and stderr handling. Preserve that streaming behavior in all process-execution code.

The only narrowly permitted Rust exception is a test that must use raw `std::process::Command` because `running_process::NativeProcess` would change the behavior under test. Such a test requires a documented, filename-specific exemption in `ci/banned_imports.py`; do not add an exemption for ordinary tests or production code.

### Waiting: background + notify, never a loop

Never poll. Select a wait mechanism from declared runtime capabilities.
With `exec_command` / `write_stdin`, start once and wait with empty input on
the same live session. With supported background tasks, wait for completion
notification. A bounded foreground waiter is also valid. See
[watch recovery](docs/architecture/hook-dispatch.md#watch-recovery-and-continuation-2004)
for capability verification, exit handling, completion scope and goal resume.
clud's `block_bad_cmd` hook refuses hand-written waits for the main agent and
every sub-agent (`while`/`until`, a `for` loop that sleeps, a `sleep` chained
to a `gh` status read, `tail -f`, `watch`); use the supported session/background
wait, or a tool with its own deadline (`clud ci`, `bosn ci run --wait`, `pr_merge_watch`)
([#1880](https://github.com/zackees/clud/issues/1880),
[DD-168](docs/DESIGN_DECISIONS.md#dd-168-hand-written-waits-are-refused-a-short-standalone-sleep-is-not)).

### One binary: `clud` is multicall

clud ships exactly one executable, `clud`, so installation stays trivial. At
launch it installs itself under every name it serves (`clud-cmd-scan`,
`clud-shim`, `python`, `pip`, `rm`, `gh`, …): hardlink first, then symlink,
then a plain copy, so the setup never fails because a link could not be made.
It dispatches on argv[0] before any other startup work. Do not add a new
`[[bin]]`, a separately packaged helper, or a new entry in the wheel's script
list. A new function gets a dispatch name in `clud` instead. Test-only
binaries that never ship (such as `clud-ctrlc-probe`, pruned from the wheel)
are exempt. The
separate binaries are gone ([#1551](https://github.com/zackees/clud/issues/1551),
[DD-121](docs/DESIGN_DECISIONS.md#dd-121-helper-executables-are-argv0-aliases-of-the-one-clud-binary));
`tests/test_build_wheel.py` fails if a second shipped `[[bin]]` appears.

### Interpreter name: `python`, never the versioned name

Write `python` in hooks, scripts, shebangs, tests and docs. clud's shim
installs both names and resolves them to one interpreter, but only `python`
exists on every platform: stock Windows has no real versioned binary.
`ci/banned_python3.py` fails `bash lint` on the versioned name and prints
why. The shim modules are exempt. For a name outside clud's control (a
distro package, a container image, the pre-clud `install`), mark the line
`python-name-lint: allow`, or put `python-name-lint: allow-next-line` above
a line that ends in a `\` continuation.

### Host-specific code: `platform/` only

Name a host (`#[cfg(windows)]`, `cfg!(unix)`, `target_os`, ...) or a native
API crate (`libc`, `windows_sys`, ...) only inside a platform boundary: a
crate's `src/platform/mod.rs` selector or its `windows/`/`unix/` trees.
Everywhere else, call `crate::platform::<area>` and branch on
`crate::platform::host::*`. The `enforce_platform_boundary` Dylint lint has
no baseline, no allowlist and no test exemption
([platform-boundary.md](docs/architecture/platform-boundary.md),
[DD-175](docs/DESIGN_DECISIONS.md#dd-175-the-platform-boundary-has-no-exceptions)).

### Home directory: `home::user_home` only

Resolve the user's home with `crate::home::user_home()` (`clud::home::user_home()`
from a binary). `dirs::home_dir`, `std::env::home_dir` and raw `USERPROFILE`
reads are banned elsewhere by `ci/banned_home_dir.py` (`bash lint`) and the
`ban_dirs_home_dir` Dylint lint: a second resolver disagrees on Windows
([DD-161](docs/DESIGN_DECISIONS.md#dd-161-the-users-home-directory-has-exactly-one-resolver)).

### Cross-cutting registries — extend in all required places

Several features have a "single source of truth" registry that must be updated alongside the code change. Forgetting any of these causes silent misbehavior (passthrough instead of dispatch) or surprising failures (banned-import lint, missing bundled file). The full list:

- **New top-level `Command` subcommand** → 3 places (4 if it takes a raw command):
  1. `Command` enum variant in `crates/clud-bin/src/args.rs`.
  2. Dispatch arm in `crates/clud-bin/src/main.rs`.
  3. **`subcommands: &[&str]` array in `args.rs::split_known_unknown` (~line 611)** — *gotcha*: a hardcoded list the unknown-flag-passthrough splitter uses; a missing entry routes your subcommand's argv to the backend agent as passthrough instead of dispatching it, and you get errors from the wrong layer (e.g., the backend complaining about your `--cmd` flag). Also extend `value_flags` / `bool_flags` arrays in the same function if your subcommand introduces new flags.
  4. **`SEPARATOR_OWNING_SUBCOMMANDS` in the same function** — *only* if your subcommand declares a `trailing_var_arg` argument taking a raw command after `--`. *Gotcha*: omitting it fails **silently**, not loudly — clud swallows everything after `--` as backend passthrough and your subcommand sees an empty argument vector, which reads as "the user passed no command". It was a single-valued constant while `tool run` was the only such parser; `test run` (#407) made it a list.

- **New bundled skill** (`crates/clud-bin/assets/skills/*/SKILL.md`) → `BUNDLED_SKILLS` registry in `crates/clud-bin/src/skills.rs`; frontmatter must parse via a real YAML parser. Guardrail tests: `soldr cargo test -p clud --lib skills::`. *Gotcha*: `assets/skills/` is the **only** source of truth, and `skills.rs` the only installer. clud used to have a second registry (`skill_install.rs`) with its own source tree at the repo root; both wrote the same `~/.claude/skills/` files, so each launch classified the other's output as drift, printed `updated /<name>`, and silently reverted the newer bodies ([DD-039](docs/DESIGN_DECISIONS.md#dd-039-bundled-skills-have-exactly-one-source-of-truth)). Do not add a second installer or a second skill tree — `ci/banned_skill_sources.py` fails `bash lint` if you do. When retiring a skill after users may have installed it, delete the asset dir, remove the bundle entry, and add its old name to `PURGED_BUNDLED_SKILLS` in the same file ([DD-040](docs/DESIGN_DECISIONS.md#dd-040-clud-pr-clud-fix-clud-do-and-clud-pr-merge-are-retired-in-favor-of-goal)); the purge only deletes a `SKILL.md` that still carries the `managed-by: clud` marker, and it sweeps every backend's skills dir, not just `~/.claude`.

- **Any requested skill creation or update** → ask the user whether the skill should be automatically invocable or explicit-only before choosing its invocation policy, unless the request already specifies the answer. Apply the choice to the bundled source and installed copy, including Claude's `disable-model-invocation` frontmatter and Codex's `agents/openai.yaml` policy where relevant. Check that the installer will preserve the choice on the next launch. When patching an installed copy ahead of a release, keep its `managed-by: clud` marker so the next upgrade replaces the patch; removing it freezes that copy forever ([#2483](https://github.com/talos-hq/clud/issues/2483)).

- **New bundled tool / hook** (`crates/clud-bin/assets/tools/<group>/*.py`) → `BUNDLED_TOOLS` array in `crates/clud-bin/src/tools.rs` with `include_str!` of the asset. Add a `bundled_includes_<tool>` guardrail test mirroring the existing ones (e.g. `bundled_includes_pr_merge_watch`, `bundled_includes_telemetry_hook`) so a future rename or removal doesn't silently break consumers. When retiring a managed bundled tool after users may have installed it, remove the bundle entry and add its old relative path to `PURGED_TOOLS` in `crates/clud-bin/src/tool_install.rs`; the purge only deletes files that still carry the `managed-by: clud` marker.

- **New server-side setting** (`crates/clud-bin/assets/server-settings.json`) → a `Section` type plus one `SectionSpec::of::<T>()` entry in `SECTIONS` (`crates/clud-bin/src/server_settings/sections.rs`), and its built-in value in the JSON. *Gotcha*: that JSON file is also what every installed build fetches from `main`, so an edit ships to users when it merges, not when a release is cut. The guard tests fail if the file is not strict JSON, or if a registered section is missing, invalid, or unregistered. Never bump `schema_version` for an additive change: older builds would ignore the whole document. See [`docs/architecture/server-settings.md`](docs/architecture/server-settings.md).

- **New interactive selector / picker** → implement `selector::Selector` (`crates/clud-bin/src/selector.rs`): supply a `View` and handle `Key`s, and let `selector::run` own the terminal. *Gotcha*: never enable raw mode, read events, or write `writeln!`/escape sequences in the selector's own module. Raw mode clears `OPOST` on POSIX, so a bare `\n` walks the menu diagonally, and this bug shipped twice from per-module copies (#1063, #1195). Add the module to `migrated_selectors_never_drive_the_terminal_themselves` in `selector.rs`. See [DD-073](docs/DESIGN_DECISIONS.md#dd-073-every-inline-selector-renders-through-one-component).

- **New model provider** (Anthropic-compatible, API-key) → 5 places:
  1. `ModelProvider` variant and its `ALL` entry in `crates/clud-bin/src/backend.rs`; fix every
     match the compiler flags. `model_provider_all_has_no_duplicates_and_matches_variant_count`
     fails if `ALL` misses it, and `every_model_provider_round_trips_through_settings_str` if
     the settings string does.
  2. The `--<provider>` clap flag in `args.rs`, symmetric in every `conflicts_with_all`, plus
     `AuthProvider` (a separate enum the compiler does **not** force) and the
     `split_known_unknown` flag list.
  3. A descriptor row in `provider_registry::ANTHROPIC_COMPAT_PROVIDERS`, with its vault
     identifiers as `provider_auth` constants and a frozen-identifier test like
     `kimi_vault_identifiers_are_frozen_for_credential_continuity`.
     `every_descriptor_resolves_to_itself_and_appears_once` catches a duplicate row.
  4. Catalog rows in `provider_catalog::MODELS` (wire prefix, efforts, contexts,
     `claude_compact_window`, `discovery_id` in the reserved `clud-claude-*` namespace).
     `inferred_provider_from_wire_matches_representative_ids` catches a missing prefix.
  5. The unified gateway needs **no new fields**: `UnifiedGatewayConfig::with_route` takes
     the key. Probe its vault in `foreground_runtime.rs`'s unified startup, add its
     `ConversationRoute`, and map it in `failover::route_for`
     (`every_catalog_provider_has_a_gateway_route` fails otherwise).
  See [DD-093](docs/DESIGN_DECISIONS.md#dd-093-anthropic-compatible-providers-are-descriptor-rows-and-the-gateway-routes-them-as-one-list).

- **Changing reap/spare logic** → decisions must be expressible against injected
  `ProcessFacts` (unit-testable, cross-platform). Add the case to the Tier 1
  decision table in `job_orphan_reaper`'s `lifecycle_tests` first, asserting
  **spare + reason** rather than just the outcome; reach for an integration test
  only if a real Job Object or real detachment is the thing under test (budget:
  ≤5). *Gotcha*: never conclude a daemon marker is unused by grepping this repo —
  `RUNNING_PROCESS_IS_DAEMON` is set by **other programs** (zccache, soldr) via
  `running-process`, and a draft of #673 nearly deleted the spare-list on exactly
  that reasoning. Daemon-stub tests need raw `std::process::Command` and must be
  added to `ci/banned_imports.py`'s exempt set per the bullet below, because
  `NativeProcess` would set the very marker whose absence is under test. See
  [`docs/architecture/process-reaping.md`](docs/architecture/process-reaping.md).

- **Test that needs raw `std::process::Command`** → add the test filename to the exempt set in `ci/banned_imports.py`. The lint enforces that production subprocess execution goes through `running_process::NativeProcess`; exemptions exist for tests that deliberately need raw spawning because `NativeProcess` would attach a `Containment::Contained` Job Object that masks what's being tested. If your test errors with `BANNED — use running_process::NativeProcess instead`, decide whether `NativeProcess` would distort the test; if yes, add yourself to the exempt set with a comment explaining why.

- **soldr version policy** → rolling latest everywhere; there is nothing to bump. A soldr version in clud settings (`rust.version`, `optimize.rust.soldr_version`) is ignored with a yellow warning, never honored ([DD-173](docs/DESIGN_DECISIONS.md#dd-173-soldr-cannot-be-pinned-a-version-setting-is-ignored-with-a-warning)). `pyproject.toml` declares `requires = ["soldr"]` (unpinned build backend), `setup-soldr` steps under `.github/` pass no `version:`, `./install` defaults to `latest`, and the bundled Docker helper uses `ARG SOLDR_VERSION=latest` (asserted by a literal in `crates/clud-bin/src/tools.rs`). `tests/test_packaging_metadata.py::test_soldr_release_policy_moves_in_lockstep` fails if any of these reintroduces a numeric pin, so one path cannot fossilize on an old protocol (an ancient 0.8.44 pin broke catalogue setup, #1026). *Gotcha*: every `setup-soldr` job resolves "latest" via a GitHub API release lookup, so anonymous local `act` runs can exhaust the 60 req/hr limit (403). See [DD-120](docs/DESIGN_DECISIONS.md#dd-120-soldr-follows-rolling-latest-everywhere-superseding-dd-020).

## Test Coverage

- ~1100+ Rust tests (unit + integration) across arg parsing, command building, backend resolution, loop-spec, daemon HTTP, registry guardrails, and end-to-end flows.
- ~185 Python tests, mostly `--dry-run` subprocess calls plus a smaller integration set.
- Python integration tests run end-to-end against [`mock-agent`](testbins/mock-agent/README.md), including the `clud loop` DONE/BLOCKED marker contract.

## CI

Build once per target triple **on Linux**, then execute the result on native
runners that have no Rust toolchain at all. Full design and rationale:
[`docs/architecture/ci.md`](docs/architecture/ci.md).

Primary entrypoint: `.github/workflows/ci.yml`; the existing installer
acceptance workflow also observes PR events but runs jobs only with `ci-full`.
**Do not add new GitHub Actions workflow files** (`.github/workflows/*.yml`
or `*.yaml`) unless the user specifically requests them; otherwise extend the
existing entrypoints and reusable workflows. Installer
platform acceptance runs only during the release cycle or on a PR explicitly
labeled `ci-full` (legacy `ci:full` is equivalent), never on routine PR commits.
When editing its triggers, check every job, including catalog/site unit and
aggregate jobs, for the same gate.
Routine PRs and `main` updates run Linux x64 build + unit tests only. Use literal
`ci-test` for Linux integration plus Windows x64, or `ci-full` for all six
targets, including both hosted macOS architectures, and Dylint; existing
`ci:full` labels remain equivalent. `ci-windows` runs the routine Linux x64
lanes plus the Windows x64 build and suites for fast Windows iteration;
`ci-macos` does the same with both macOS triples instead. `CI OK` gates on
all of them (#1652). `main` has a **merge queue** (#2048): PR CI tests the head SHA, and the
queue's `merge_group` run tests the merged tree with the routine `minimal`
lanes (#2125). To submit a reviewed PR use `clud tool run github/pr_merge.py --
<PR> --repo <OWNER/REPO> --match-head-commit <full-head-SHA>`; it selects the
queue mutation when the base requires a queue and never bypasses protections.
Then use `github/pr_merge_watch.py` to wait for the actual merge. Manual full CI pins every job to a verified
candidate SHA. See
[`ci.md`](docs/architecture/ci.md#current-ci-selection).

**Choose labels from the changed implementation, not the callers.** Business
logic calling a platform API stays on routine CI. Do not add `ci-full` for a
shared PTY pump refactor, helper extraction, unchanged OS calls/branches, or
because a local run failed to complete. Reserve native tiers for changes to
platform adapters, native API/FFI handling, OS-specific behavior or platform
runtime contracts. Name that change and what the extra native lanes prove in
the PR body; choose the smallest tier covering the affected platforms
(`ci-windows` for Windows-only implementation changes, `ci-macos` for
macOS-only ones, `ci-full` when the full platform set is affected). Release validation remains full.
See [label selection](docs/architecture/ci.md#when-to-add-native-ci-labels).

| File | Role |
| --- | --- |
| `.github/actions/setup-build/action.yml` | python + uv + soldr + cross tooling. Build side only. |
| `.github/actions/setup-exec/action.yml` | python + uv, and **removes** the Rust toolchain. Exec side only. |
| `.github/workflows/_build-target.yml` | one triple → one test bundle (+ optional wheel). The only workflow that compiles Rust. |
| `.github/workflows/_run-tests.yml` | one triple × one suite → execution, no compilation. |
| `.github/workflows/_dylint.yml` | Linux-only nightly lint; off the PR path. |
| `ci/ci_matrix.py` | the triple → {build host, cross strategy, exec runner} table. |
| `ci/xbuild.py` | every cargo/maturin invocation, plus the per-strategy cross env. |
| `ci/bundle.py` / `ci/run_bundle.py` | pack the bundle / execute it on the exec runner. |

Things that bite:

- **Only `auto-release.yml` may build `--release`.** `_build-target.yml` fails
  the job if any other workflow passes `profile: release`.
- **Never use `uv run` in a workflow step.** `pyproject.toml` sets
  `build-backend = "soldr"`, so `uv run` syncs the project and triggers a full
  PEP 517 maturin build. Use `$VENV_PY` (exported by both composite actions),
  which is what `lint` already does locally.
- **soldr owns Apple/MSVC cross builds (#637, #714).** Never invoke or install
  `cargo xwin`, `cargo zigbuild`, `zig cc`, `cross` or `osxcross` for a
  `*-apple-darwin` / `*-pc-windows-msvc` target — use `soldr prepare` /
  `soldr build`. As of soldr 0.8.40, soldr **also owns `*-unknown-linux-gnu`**:
  its catalogue GNU toolchain (`gcc-13.3.0-glibc-2.17-1`, soldr#2238) replaced
  zig for Linux and the manylinux wheel, so `ci/xbuild.py::is_soldr_owned`
  covers linux-gnu and `cargo_argv` refuses zigbuild for it too. clud no longer
  invokes zig anywhere.
  The static libstdc++/libgcc link mechanism whisper-rs-sys needed is **gone**
  (#1207): no C++ `-sys` crate remains in `Cargo.lock` (`ring` and `blake3` are
  C), so soldr's catalogue sysroot is the entire manylinux_2_17 floor. Do not
  reintroduce `WHISPER_LINK_CXX_STATIC` or static-C++ RUSTFLAGS without a real
  C++ dependency to justify them. Changing these flags is gated on a green
  release wheel build (`python -m ci.xbuild wheel --target
  x86_64-unknown-linux-gnu --strategy soldr --profile release`), which no PR
  workflow runs; see
  [`ci.md`](docs/architecture/ci.md#the-static-c-runtime-link-is-gone-1207).
  `ci/banned_cross_tools.py` enforces the ban under `bash lint` and CI's static
  checks job; `ci/xbuild.py::cargo_argv` additionally *raises* on a zigbuild strategy
  for any soldr-owned triple (which is now every clud triple), because the text
  scan cannot follow a target held in a variable.
  Two rule classes, and the distinction is the thing to get right when editing
  it: **unconditional** — flagged wherever they appear, no target consulted:
  `cargo xwin`, the bare `xwin` CLI, `XWIN_*`, `osxcross`, `cross build`,
  `Cross.toml`, **and now every zig invocation** (`cargo zigbuild`, `maturin
  --zig`, `zig cc` — banned at every target since soldr#2299, because soldr's
  catalogue toolchain replaced zig's last Linux use) — versus
  **conditional on a soldr-owned triple** — now only the hand-rolled
  `[target.<triple>] linker =` TOML rule. Installs are matched against the whole file, so
  a `taiki-e/install-action` step with `tool: cargo-xwin` two lines below is
  caught, and an install suppresses the invocation rules on its own line so one
  mistake is not counted twice. Scope: `.github/`, `ci/`, `bench/`, `crates/`,
  `dylints/`, `testbins/`, `tests/`, `.claude/hooks/`, the root
  entrypoints and `.cargo/config.toml` (`vendor/` is deliberately out).
  *Gotcha*: prose that explains the ban must not trip it — the scanner strips
  `#`, and `//` + `/* */` in Rust, and conditional rules still require a
  concrete triple (`x86_64-apple-darwin`, not `*-apple-darwin`). For prose a
  comment-stripper cannot see, such as a module docstring naming `cargo xwin`,
  put `cross-lint: allow` **on that same line** — the marker is line-scoped, so
  putting it on a docstring's closing line suppresses nothing. `rg` for the
  marker lists every escape in the tree. See
  [`docs/architecture/ci.md`](docs/architecture/ci.md).

- **Adding a target** means editing `ci/ci_matrix.py` *and* adding the
  build/test job pair in `ci.yml`. They cannot be one matrix (GitHub `needs:`
  on a matrix is all-or-nothing, which would serialize every lane behind the
  slowest); `tests/test_ci_matrix.py` fails if the two drift apart.

## Design principles (fleet directives, zackees/ci.yml#163)

Agent and review guidance from zackees/ci.yml `docs/policy-rust.md` ("Design principles, narrowed"). Not enforced by ci-lint; no rule ID.

1. **Search before you add.** Before adding a resolver, detector, predicate, guard or path helper, `rg` for an existing one. Reuse it, or say in the PR why it does not fit.
2. **Conflicting implementations get a research issue, not an inline winner.** If the same concept is implemented more than once with observably different behaviour and fixing it needs a design decision, file `research: N conflicting implementations of X -- which is canonical?` listing every copy (`file:line`) and what the difference does to real input; cross-reference it and carry on with your task. Copies that behave identically are a refactor, not an issue.
3. **Keep behaviour changes and refactors apart.** Separate commits, preferably separate PRs, never one mixed diff.
4. **Refactor first only when you would grow a giant function.** If a fix would add more than 50 lines to a function already over 200 lines, extract first in a separate behaviour-preserving change with characterization tests written first (RED -> GREEN). Otherwise fix it where it belongs; no unrequested restructures.
5. **Function and file size follow the complexity ratchet (`RUST-018`).** New functions stay under clippy's `too_many_lines` (100) and `cognitive_complexity` (25). A function carrying an `#[expect]` must not grow; delete the `expect` once it is back under. Never split a file with `include!` fragments to dodge a length ceiling. Before splitting a hot file, check `git branch -a --contains` for in-flight branches -- a split is a rename and conflicts destructively.
6. **Fail fast, and delete before you abstract.** Reject invalid input at the boundary with a typed error; never fall back silently to a degraded path. Prefer deleting code to adding a layer. No new trait until there are two real implementations (a test double does not count).
7. **Existing boundaries beat generic DRY.** Platform modules, Dylint-enforced layering, crate boundaries and documented deliberate copies win over removing duplication. Do not copy code across a crate boundary to get around it -- say the boundary hurts. An undocumented copy is a finding under 2; a documented one is not.

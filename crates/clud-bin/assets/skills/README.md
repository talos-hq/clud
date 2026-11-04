# skills/

Claude Code and Codex skills bundled into the `clud` binary as compile-time
assets. During global launch setup, the installer copies each skill into the
selected backend's skill directory (`~/.claude/skills/` for Claude,
`~/.codex/skills/` for Codex when Codex is present). Session-only launches
do not write persistent skill files. Stale clud-managed copies under
`~/.agents/skills/` are purged only during Codex global setup.

Investigation skills link to
[bounded-output.md](../../../../docs/architecture/bounded-output.md) in one
line instead of copying the practice.

Retired skills are listed in `skills::PURGED_BUNDLED_SKILLS` and swept out of
every backend's skills dir on launch. `clud-loop` was retired once
`clud --codex --harness claude` gave Codex models the harness's native `/loop`.

## Skills

- [clud-gate/](clud-gate/SKILL.md) - Compact lint/test validation and failure logs.

- [grind/](grind/SKILL.md) - Router for the `/grind` DAG: intake, mode,
  per-role models and local-CI questions, then the bundled `grind-run` workflow.
  Leaves: [grind-intake/](grind-intake/SKILL.md),
  [grind-prework/](grind-prework/SKILL.md),
  [grind-plan/](grind-plan/SKILL.md), [grind-work/](grind-work/SKILL.md),
  [grind-review/](grind-review/SKILL.md),
  [grind-integrate/](grind-integrate/SKILL.md),
  [grind-land/](grind-land/SKILL.md), [grind-cron/](grind-cron/SKILL.md).
  Contract: [grind.md](../../../../docs/architecture/grind.md). It replaces
  the retired `clud-meta-work`.
- [clud-issue/](clud-issue/README.md) - File a researched GitHub issue without
  an interview, or roll existing issues into a verified native meta-issue.
- [clud-issue-triage/](clud-issue-triage/README.md) - Triage GitHub issues:
  close ones that are clearly resolved and silently file follow-ups for
  un-addressed CodeRabbit comments; supports single, last-week, or all.
- [clud-tag-release/](clud-tag-release/README.md) - Tag a release after
  validating version match, clean `main`, and no duplicate tag, then push and
  surface the auto-release workflow URL.
- [clud-docker-rust-app-dev/](clud-docker-rust-app-dev/README.md) - Build a
  Rust app inside Docker for development iteration, not deployment. It uses
  fast incremental cargo builds via named volumes for `target/` + `CARGO_HOME`
  + `RUSTUP_HOME`, source bind-mounted, soldr-wrapped cargo, and a Python
  orchestrator.
- [clud-improve/](clud-improve/SKILL.md) - File concrete clud improvement
  reports directly as GitHub issues against `zackees/clud`; ask for details
  only on a bare manual `/clud-improve` invocation.
- [clud-omarchy/](clud-omarchy/SKILL.md) - Route Omarchy and Hyprland desktop
  configuration tasks to current official documentation and maintained agent
  skills without bundling a stale copy of their configuration syntax.
- [clud-docker-recover/](clud-docker-recover/SKILL.md) - Diagnose and recover
  a wedged Docker Desktop (engine pipe/socket absent, WSL/Docker startup
  failures) via the bundled `docker/docker_recover.py` tool. Read-only
  `doctor` first; confirmation-gated `restart`/`reset`; Windows storage disks
  resolved from Docker Desktop's real config (never the assumed C: default)
  and never compacted or deleted automatically.
- [video-use/](video-use/SKILL.md) - Explicit-only `/video-use` bridge to the
  pinned browser-use/video-use checkout via `clud video --path`; no context
  cost until invoked. Contract:
  [launch-targets.md](../../../../docs/architecture/launch-targets.md#clud-video-video-use-as-a-session-scoped-plugin-1851).

## Platform coverage selection

Planning, review and integration skills select additional native coverage
from changed platform implementations, not callers or available target lists.
The integrator's `git/ci_targets.py` result is an inventory; it does not
require checking every target for shared business-logic changes. Keep the
repository's routine checks and complete release validation. Repository-local
PTY skills follow [the same selection rule](../../../../docs/architecture/ci.md#when-to-add-native-ci-labels).

Implementation skills follow the [shared landing contract](../../../../docs/architecture/launch-plan.md#landing-preference).
Filing and build skills offer scoped next steps; their invocation policies are unchanged.

## How Skills Ship

Each `SKILL.md` here is embedded into the binary via `include_str!` and written
out during global setup. One installer, registered behind `launch_setup.rs`:

- **`crates/clud-bin/src/skills.rs`** - selected-backend global setup
  (`~/.claude/skills`, `~/.codex/skills` gated by `~/.codex`), never
  overwrites user edits, reads from this directory.

See
[docs/architecture/skill-system.md](../../../../docs/architecture/skill-system.md)
for the installer model and rationale.

## Adding a Skill

See the checklist in
[docs/architecture/skill-system.md](../../../../docs/architecture/skill-system.md#adding-a-skill).
Register in `skills.rs::BUNDLED_SKILLS`.

`clud-secret` acquires credentials through the masked native prompt and direct
sinks; see [secret-prompt.md](../../../../docs/architecture/secret-prompt.md).

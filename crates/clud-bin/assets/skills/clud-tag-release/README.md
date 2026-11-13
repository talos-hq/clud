# clud-tag-release/

Bundled skill for observing clud’s automatic version-tag handoff, recovering a missing handoff, and manually tagging releases in other tag-driven Rust/Python repositories. Normal clud version-bump merges need no human tag push; [CI architecture](../../../../../docs/architecture/ci.md#automatic-version-tags-2764) owns the automation and recovery contract.

## Files

- `SKILL.md` - Skill frontmatter, triggers, normal-path observation, explicit recovery, and the generic manual fallback.

## How it ships

The `SKILL.md` body is embedded into the `clud` binary at compile time via `include_str!` from `crates/clud-bin/src/skills.rs` (see the `BUNDLED_SKILLS` array). During global setup, `skills::ensure_installed` mirrors the payload for the selected detected backend (`~/.claude/skills/` or `~/.codex/skills/` gated on `~/.codex`) without overwriting an existing file. Install failures are non-fatal: they log a `[clud] note: ...` line and launch proceeds.

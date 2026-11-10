# assets/

Compile-time-embedded resources bundled directly into the `clud-bin` binary. Files here are read by `include_str!` during the Rust build, so the shipped binary carries them in its read-only data segment and never touches the filesystem to locate them at runtime.

## Layout

- `agents/` and `workflows/` - Claude Code agent types (`grind-*`) and the `grind-run` workflow, installed into `~/.claude/` by `src/claude_files.rs`; [grind.md](../../../docs/architecture/grind.md) owns forge identity, role caps and watch recovery.
- [`skills/`](skills/README.md) - slash-command skill definitions (`SKILL.md` files) installed per-backend into `.claude/skills/` or `.codex/skills/` on first run.
- [`tools/`](tools/README.md) - embedded scripts and the `clud tool run` watchdog exit contract.
- `fonts/` - `DejaVuSansMono.ttf` for rasterizing toast text into the kitty-graphics toast image (#1189, `src/toast/raster.rs`, embedded with `include_bytes!`). Bundled so the wheel needs no system font library; its license is `fonts/DejaVu-LICENSE.txt`.
- `server-settings.json` - server-side settings (#1192, `src/server_settings/`). The binary embeds this file as its built-in copy, and installed builds also fetch it live from `main`, so an edit here reaches users when it merges, not when a release ships. Guard tests require strict JSON and a valid value for every registered section. See [`docs/architecture/server-settings.md`](../../../docs/architecture/server-settings.md) and [DD-072](../../../docs/DESIGN_DECISIONS.md#dd-072-server-side-settings-are-one-baked-in-json-document-with-per-section-last-known-good).
- `codex-models.json` - embedded stable Codex slug snapshot refreshed from OpenRouter; [model-pages.md](../../../docs/architecture/model-pages.md) describes publication and account checks.
- `lokr-release.json` - functional lokr release assets pinned by target, URL, exact size and SHA-256. `ci/lokr_bundle.py` prepares embedded bytes for native installers and verified download identities for release wheels; [secret-prompt.md](../../../docs/architecture/secret-prompt.md) owns private installation and resolution.
- `openrouter-catalog.json` - baked-in fallback for the live OpenRouter pricing catalog (#1256, `src/openrouter_catalog.rs`). The consumer prefers its validated daemon-state cache and fetches a fixed allowlisted catalog origin; this embedded seed keeps the shortlist available on first use and during outages.

## Embedding mechanism

Text assets are referenced from Rust source via `include_str!`, including [`src/skills.rs`](../src/skills.rs). The macro inlines file contents at compile time, so asset changes require a rebuild through `soldr cargo`. Published builds use `ci.xbuild` to prepare the pinned lokr bytes for `include_bytes!`; the native installer extracts that companion privately, independently of clud's install location.

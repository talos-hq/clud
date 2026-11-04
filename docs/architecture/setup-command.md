# `clud setup`

`clud setup` takes a machine from a downloaded `clud` to a working one in one
command (#2219). It owns no install logic of its own; each step calls the code
another entry point already uses.

| Step | What it does | Implementation |
| --- | --- | --- |
| 1. Aliases | Links the interpreter aliases (`python`, `pip`, ...) under `~/.clud/state/shims/` to the running `clud` | `shim_install::extract_shims_at`, the same call every launch makes |
| 2. Provider | OpenRouter, Claude or Codex; stores an OpenRouter key; picks an OpenRouter model; saves the provider and native harness as defaults | `self_install::setup` (the flow drafted in #2223) |
| 3. Shell activation | Only with `--write-profile`: runs the native installer for this version, which owns every shell-profile and PATH edit | `self_install::entry::install_current_with_profile` |

Code: [`setup_command.rs`](../../crates/clud-bin/src/setup_command.rs),
[`self_install/setup.rs`](../../crates/clud-bin/src/self_install/setup.rs).

## Flags and exit codes

```
clud setup [--dry-run] [--yes|-y] [--provider claude|codex|openrouter] [--write-profile] [--import-hermes]
```

- `--dry-run` prints every step and what is already in place, and writes
  nothing (no aliases, settings, credential or lock files). It works without a
  terminal.
- `--yes` runs without prompts. Without `--provider` it keeps the saved
  provider, or Claude when none is saved. An OpenRouter key then comes from
  the vault or `OPENROUTER_API_KEY`; with neither, setup exits 2 and says so.
- Without a terminal and without `--yes`, setup exits 2 at once with a
  message naming `--yes`. It never waits for input it cannot get.
- `--provider` skips the provider menu.

Exit 0 on success, 2 for a refused request, 1 when a step fails.

## Idempotency

Each step checks first and reports what it found: `aliases: all N already
installed`, `credentials: OpenRouter key already stored`, `provider: already
claude`. A rerun changes nothing that is already in place.

## What it does not touch

Without `--write-profile`, setup never edits a shell profile, the User PATH,
or anything outside `~/.clud`, matching the installer's
`--no-modify-path` contract ([native-installer.md](native-installer.md)).
Claude and Codex keep their own subscription logins; setup stores no
credential for them. A credential value is never printed: the OpenRouter key
goes from the masked prompt or the environment straight into the existing
credential store (`provider_auth`).

## Why the installer does not chain into setup

#2223 ran provider setup at the end of an interactive install. The
installer's fresh-shell activation probes share the controlling terminal, so
a selector opened after them fails with EIO. Fixing that needs the probes
moved to isolated PTYs. Until then, the installer stays as it is, and
provider setup has a single entry point: `clud setup`.

## Importing Hermes OpenRouter settings (setup only)

Before any other step, setup looks for OpenRouter settings Hermes already
has and **offers** to import them. This is a security boundary: `clud setup`
is the only command that reads Hermes config or its environment. Launches,
`run`, `serve`, the daemon and `doctor` never read `~/.hermes`, a Hermes
profile, its `.env` or `HERMES_HOME`, so a normal launch cannot pick up
someone else's credentials. The reader is the private module
[`setup_command/hermes.rs`](../../crates/clud-bin/src/setup_command/hermes.rs);
a unit test fails if any other `src/` file names a Hermes path or variable,
and `hermes_config_is_invisible_to_every_command_but_setup` runs normal
commands against an invalid-YAML Hermes config to prove none of them parse it.

- Sources, read-only, first match wins: `$HERMES_HOME/config.yaml`,
  `~/.hermes/config.yaml`, `~/.hermes/profiles/*/config.yaml`. Read:
  `provider`/`model.provider`, `base_url`/`model.base_url`, `model.default`,
  `model_overrides.openrouter`, `providers.openrouter`. An empty `base_url`
  means OpenRouter's default endpoint.
- The key: `OPENROUTER_API_KEY` from the environment, else from Hermes'
  `.env` beside the config. It is never printed, only its source.
- Setup prints the config path, base URL, model ids and key source, then asks
  `Import these OpenRouter settings into clud? [y/N]` (default no).
- `--yes` never imports unless `--import-hermes` is also given.
  `--dry-run` shows what would be imported and writes nothing. An invalid
  config is reported and skipped; with no Hermes config, setup is unchanged.
- An import prints the metered pay-per-token warning and still requires
  `CLUD_ALLOW_METERED=1` and `CLUD_METERED_PROVIDER=openrouter`. Settings
  without a key fail with exit 2 naming `OPENROUTER_API_KEY`. Either refusal
  writes nothing. An accepted import stores the key through the credential
  store, saves the first model as the OpenRouter default and makes
  OpenRouter the default provider.

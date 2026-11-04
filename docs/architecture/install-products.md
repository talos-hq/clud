# `clud install <product>` — installing other agent products

`clud install hermes` and `clud install talos` install a *product*: a separate
agent application that clud does not own but must hand a working
configuration to. `clud setup` (#2219,
[setup-command.md](setup-command.md)) sets up **clud**; `clud install` puts a
**product** on the machine and then configures it. The two never merge: a bare
`clud setup` keeps its current meaning, and `clud setup <product>` is an exact
alias of `clud install <product>`.

Canonical spelling is `install`. The command's job is installing a product, and
`install` reads correctly in the log lines an operator looks for
(`install: fetched hermes bootstrap`, `install: migrated 1 provider`). `setup`
stays as the alias because the products' own entry points are called
`setup` (`hermes setup clud`), and an operator who has just read that page
should find the clud side under the same word.

## Products

The registry is a table in `crates/clud-bin/src/install/products.rs`; it holds
no logic beyond product identity, so adding a product does not touch the
runner.

| Target | Product | Bootstrap | Default dir | Config the configure step writes |
| --- | --- | --- | --- | --- |
| `hermes` | hermes-slim (`talos-hq/hermes-slim`) | `slim/install.sh` — downloads one prebuilt bundle and checks its `.sha256` | `~/.hermes-slim` | `$HERMES_HOME/config.yaml` |
| `talos` | talos (`talos-hq/talos`) | `scripts/install.sh` — clone plus bootstrap | `~/.hermes` | `$HERMES_HOME/config.yaml` |

Both bootstraps are shell scripts fetched from the product's repository at a
pinned ref and cached under `~/.clud/cache/install/<product>/`. The fetch is
capped, and the script's sha256 is printed so an operator can compare it with
the repository's own copy. `--ref` chooses another ref; the default is the
product's default branch. There is no compiled-in version of the product:
`--version` is passed straight through to the product's own installer.

## Grammar

```
clud install [--dry-run] [--yes|-y] [--dir DIR] [--target DIR] [--ref REF]
             [--version V] [--no-configure] [--configure-only]
             [--migrate-credentials] <PRODUCT> [-- INSTALLER_ARGS...]

clud setup   ... same options ... <PRODUCT> [-- INSTALLER_ARGS...]
```

Exit codes match `clud setup`: `0` success, `2` refused or impossible request
(unknown product, no terminal without `--yes`, missing prerequisite), `1` a step
failed. Every step reports what it found, so a rerun is safe.

- `--dry-run` prints the resolved product, the bootstrap URL, the target
  directory, and the provider settings that would be migrated. It fetches
  nothing, runs nothing, and writes nothing. Works without a terminal.
- `--configure-only` skips the bootstrap and runs only the configure step.
  This is what `hermes setup clud` and `talos setup clud` call.
- `--target DIR` names an existing install to configure, for
  `--configure-only` and to skip the wizard's directory question. It is the
  same value `--dir` gives the installer, and the two are mutually exclusive.
- `--no-configure` installs and stops.
- `--yes` runs without prompts; it never migrates credentials unless
  `--migrate-credentials` is also given.
- Without a terminal and without `--yes`, the command exits `2` at once naming
  `--yes`. It never waits for input it cannot get.

## Steps

1. **Resolve.** Unknown target exits `2` and lists the known targets.
2. **Fetch.** Download the bootstrap into the cache, refusing anything larger
   than the size cap, and record its digest.
3. **Install.** Run the bootstrap with the caller's terminal inherited, so the
   product's own setup wizard sees a tty, with `INSTALLER_ARGS` appended.
   `--dir` is translated into the product's own directory variable
   (`HERMES_SLIM_DIR` / `HERMES_INSTALL_DIR`), never into a flag the product
   might not have. The script runs through its own shebang — hermes-slim's is
   `#!/bin/sh`, talos' is `#!/usr/bin/env bash`, so naming one interpreter would
   break the other — and the extra arguments are appended as-is, because a bare
   `--` is hermes-slim's "hand the rest to setup" but an error for talos. The
   child's exit status is the step's verdict; its output is the caller's
   terminal, so clud reports the status and does not re-read it. The child runs
   through `running-process`, like every other spawn in clud.
4. **Configure.** This is clud's own work, below.
5. **Report.** Print the product's launch command and
   `<product> setup clud` for a later reconfigure.

## Configure: migrating clud's provider credentials

Zach's requirement: if clud already holds provider credentials, the user is
asked whether to migrate them into the freshly installed product; if it holds
none, the step is skipped and says so.

- **What is offered.** Every provider clud can read a stored key for —
  `openrouter`, `deepseek`, `kimi` — plus the provider clud is currently
  configured to use. Claude and Codex are reported as externally managed and
  never migrated, matching `clud auth` and `clud setup`.
- **The prompt.** Secret-free first. The command prints, one line per provider,
  `provider: openrouter (stored key), model: x-ai/grok-5` and then asks
  `Would you like to migrate clud credentials for providers into hermes? [y/N]`
  — default no. With no credentials it prints
  `providers: nothing stored in clud; nothing to migrate` and asks nothing.
- **What is written.** The product's own config format, never another file of
  clud's: `providers.<name>.base_url` and `providers.<name>.api_key`. Model
  selection stays the product's own choice: clud has no saved model to hand
  over on this path, so guessing one would be worse than leaving it. The write is
  an edit of an existing document, not a replacement: unknown keys, comments
  and other providers are preserved, an existing value is replaced in place,
  and a parse failure aborts with the file left byte-identical. The edit is
  staged to `config.yaml.clud-new` and renamed, so a partial write cannot
  leave a broken config.
- **A secret is never printed.** It goes from the credential store straight into
  the written file, and the report names the file and the provider, never the
  value. `--dry-run` names the file and the keys it would set.
- **A launch never migrates.** This extends the boundary in
  [setup-command.md](setup-command.md): only `setup`/`install` may read provider
  settings or another product's config. A normal launch, `run`, `serve`, the
  daemon and `doctor` read no product config.

## The product side: `hermes setup clud`, `talos setup clud`

Each product gains a `clud` section in its own `setup`, next to
`model|tts|terminal|gateway|tools|telemetry|agent`:

```
hermes setup clud [--print] [--yes]
talos  setup clud [--print] [--yes]
```

It does not reimplement configuration. It checks `clud` on `PATH` and runs
`clud install <product> --configure-only --target <dir>` with the terminal
forwarded, so migration, prompting and writing stay in one implementation and
cannot drift from `clud install`. `--print` shows the command and the clud
version it would call, and changes nothing. Without clud on `PATH` the section
prints the install line and exits `2` rather than guessing a config format.

## Why the product does not read clud's state itself

Reading `~/.clud` from a product would put clud's credential store, its
settings schema and its versioning into two more codebases, which is how the
two-sided `clud setup` import (#2606) is already kept in one place. The single
door is the clud binary the user installed.

## Code

[`install/products.rs`](../../crates/clud-bin/src/install/products.rs) (the
registry), [`install/command.rs`](../../crates/clud-bin/src/install/command.rs)
(resolve, fetch, run), [`install/configure.rs`](../../crates/clud-bin/src/install/configure.rs)
(the vault read and the config editor).

## Testing

Unit tests live with the code and run in the `clud` lib target:

- `install::products` — every target resolves to itself, lookup ignores case,
  targets are unique and lowercase, the bootstrap URL joins repo/branch/path,
  the reconfigure hint names the product's binary.
- `install::command` — unknown target refused with the known ones, `--dry-run`
  reporting the plan and writing nothing, a terminal-less install refused before
  any fetch, `--dir` and `--target` mutually exclusive, `--configure-only`
  installing nothing, the bootstrap executing through its own shebang with the
  directory variable and the extra arguments set, and a failing bootstrap
  reporting its last lines.
- `install::configure` — the OpenAI-compatible URL derived from the registry's
  Anthropic one, an empty document gaining a `providers:` block, a new provider
  joining an existing mapping with comments intact, an existing provider's
  values replaced in place, a missing key added, a document that does not parse
  refused byte-identical, the offer summary secret-free, and nothing stored
  writing nothing.

Still open: a CLI-level test that drives `clud install` and `clud setup
<product>` as processes, asserting the alias produces identical output.

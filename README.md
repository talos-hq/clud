# clud

![hero-clud](https://github.com/user-attachments/assets/4009dfee-e703-446d-b073-80d826708a10)

**A luxury agentic experience with Claude, Codex and DeepSeek, by running them all on Claude Code.**

- Fixes the Windows performance problem with zombie git/bash processes.
- Runs Codex and DeepSeek models inside the Claude Code terminal.

The name `clud` is simply a shorter, easier-to-type version of `claude`.

Using Hermes Agent? Start with [Hermes integration](#hermes-integration) for MCP setup and model gateway discovery.

## Run Codex on the Claude harness

```bash
clud --codex --harness claude
```

See [Codex through Claude Code](#codex-through-claude-code-experimental).

## Solve one issue, all the way to a merged PR

```bash
clud do https://github.com/<owner>/<repo>/issues/<N>
```

See [`clud do`](#clud-do-url-or-goal--implement-to-a-merged-pr).

## Grind down your issue list

```bash
clud grind
```

Plans, writes, reviews and lands a whole issue list as merged PRs, with each
agent role capped to its job. See [`clud grind`](#clud-grind--burn-down-issues-to-merged-prs).

[![CI](https://github.com/zackees/clud/actions/workflows/ci.yml/badge.svg?branch=main&event=push)](https://github.com/zackees/clud/actions/workflows/ci.yml?query=branch%3Amain+event%3Apush)
[![Auto Release](https://github.com/zackees/clud/actions/workflows/auto-release.yml/badge.svg?event=push)](https://github.com/zackees/clud/actions/workflows/auto-release.yml?query=event%3Apush)

CI builds each of the six target triples once on Linux and executes the result
on native Linux/Windows/macOS runners — see
[docs/architecture/ci.md](docs/architecture/ci.md).

## Installation

**macOS / Linux**

```bash
curl -fsSL https://raw.githubusercontent.com/zackees/clud/main/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/zackees/clud/main/install.ps1 | iex
```

Both scripts install [`uv`](https://docs.astral.sh/uv) if needed, then `uv tool install clud`, and put `clud` on PATH for new shells. Pin a version with `CLUD_VERSION=2.0.14 curl ... | sh` (POSIX) or `$env:CLUD_VERSION = '2.0.14'; irm ... | iex` (PowerShell). Re-run to upgrade.

Already have a Python package manager? Any of these works equivalently:

```bash
uv tool install clud   # recommended — isolated, fast
pipx install clud      # equivalent if you already use pipx
pip install clud       # plain pip; you must ensure the install bin dir is on PATH
```


## Hermes integration

Hermes Agent can discover clud through its stdio MCP server: run durable agent
sessions or start a local Claude/Codex model API. Gateway discovery tools are
available in clud 2.8.41 and later.

The MCP SDK is a base dependency, so any clud install can run the server
(`uv tool install clud`, or `python -m pip install --upgrade clud`). The
`clud[mcp]` extra still resolves but adds nothing.

Add this entry under `mcp_servers` in `~/.hermes/config.yaml`, using
[Hermes's MCP configuration format](https://hermes-agent.nousresearch.com/docs/user-guide/features/mcp):

```yaml
mcp_servers:
  clud:
    command: "python"
    args: ["-m", "clud.mcp_server"]
```

Use the installed interpreter's absolute path for `command` when Hermes uses a
different Python environment. The `clud` binary must be on that server's PATH;
set `CLUD_BIN` to its absolute path if needed. Restart Hermes or use
`/reload-mcp` to refresh discovery.

| Discoverable MCP tool | What Hermes can do |
| --- | --- |
| `serve_start` | Start a gateway with `backend="codex"` (default) or `backend="claude"`; optionally add `openrouter=true`. |
| `serve_status` | Retrieve the running gateway's URLs, protocol, authentication header, and client token. |
| `serve_stop` | Stop the gateway owned by this MCP connection. |
| `session_start` | Submit an agent task and receive a durable session handle immediately. |
| `session_status`, `session_result`, `session_wait`, `session_kill` | Inspect, collect, wait for, or stop an agent session. |

For Codex, authenticate with
`clud auth login codex --acknowledge-experimental`, or provide `OPENAI_API_KEY`
to the MCP server. Claude requires `ANTHROPIC_API_KEY`; optional OpenRouter uses
`clud auth login openrouter`. Keep provider credentials in the server's
environment or credential store.

Example instruction to Hermes:

> Use clud's serve_start with backend codex, then show the base URL and model
> discovery URL. Keep the client token private.

`serve_start` returns connection details for an **Anthropic Messages** API:
`base_url`, `models_url`, `messages_url`, `auth_header`, and `client_token`.
Use the listed model IDs and returned client authentication for inference.
The MCP connection owns the gateway and stops it on disconnect. For agent
tasks that must survive a tool-call deadline, use the durable `session_*`
tools. See [gateway setup and lifecycle](docs/architecture/standalone-gateway.md)
for the complete contract and HTTP examples.

## Usage

```bash
clud                              # Launch Claude in YOLO mode
clud --codex                      # Use Codex as the backend
clud --claude                     # Use Claude as the backend (default)
clud --deepseek                   # Use DeepSeek through the Claude harness
clud --deepseek sk-...            # Same, storing that API key in the native credential vault first
clud --kimi                       # Use Kimi through the Claude harness
clud --openrouter                 # Use Claude through OpenRouter
clud --pty                        # Force PTY launch mode
clud --subprocess                 # Force subprocess launch mode
clud --pty --graphics=sixel       # Force a Sixel header above PTY output
clud --demo-gfx-sixel             # Render the README hero image as a Sixel demo
clud --detach -p "review this PR" # Start a daemon-managed session without attaching
clud --detachable -p "fix CI"     # Ctrl+C asks whether to keep the session in background
clud --transcript session.log -p "debug this" # Tee daemon session output to a file
clud -c                           # Continue the most recent conversation
clud --resume                     # Resume a session
clud --resume abc123              # Resume a specific session by ID or search term
clud -p "refactor the auth layer" # Run with a prompt, exit when done
clud -m "what does this do?"      # Send a one-off message
clud --model opus -p "review PR"  # Choose a model
clud --safe -p "drop the table"   # Disable YOLO mode (keeps permission prompts)
clud --unsafe -p "clean build"    # Disable clud's agent safety rules for this launch
clud --dry-run -p "hello"         # Print what would run without executing
echo "explain this error" | clud  # Pipe mode: read prompt from stdin
clud -- --verbose --debug         # Pass extra flags through to the backend
clud attach                       # List background sessions you can reattach to
clud attach sess-123              # Attach to a specific session
clud list                         # Show background session IDs, PIDs, and cwd
clud wasm guest.wasm              # Run a local wasm module with clud's embedded runtime
```

### Flags

| Flag | Description |
|------|-------------|
| `-p`, `--prompt` | Run with a prompt, exit when complete |
| `-m`, `--message` | Send a one-off message |
| `-c`, `--continue` | Continue the most recent conversation |
| `-r`, `--resume [TERM]` | Resume by session ID or search term |
| `--claude` | Use Claude as the backend |
| `--codex` | Use Codex as the backend |
| `--deepseek` | Use DeepSeek's Anthropic-compatible API through Claude Code |
| `--kimi` | Use Kimi's Anthropic-compatible API through Claude Code |
| `--openrouter` | Use OpenRouter's Anthropic endpoint through Claude Code |
| `--subprocess` | Force subprocess launch mode |
| `--pty` | Force PTY launch mode |
| `--graphics <auto\|off\|sixel>` | Control PTY graphics headers. `auto` only enables Sixel from a live terminal probe |
| `--graphics-image <PATH>` | Render a custom image as the PTY graphics header when Sixel is enabled |
| `--demo-gfx-sixel` | Render the README hero image as a standalone Sixel demo and exit |
| `--detach` | Start a daemon-managed session directly in the background |
| `--detachable` | Run attached under the daemon; `Ctrl+C` prompts whether to background or end |
| `--transcript <PATH>` | Tee daemon-managed session output bytes to a transcript file |
| `--model <NAME>` | Set model preference (e.g., haiku, sonnet, opus) |
| `--safe` | Disable YOLO mode (don't inject `--dangerously-skip-permissions`) |
| `--unsafe` | Disable clud's agent safety rules for this launch ([policy](docs/architecture/unsafe-mode.md)); conflicts with `--safe` |
| `-c`, `--continue` | At a terminal, pick one of this directory's Claude-harness sessions (newest first, with provider, context estimate and compact checkpoints) to resume; without a terminal, Claude's native `--continue` |
| `--last` | Resume this directory's newest Claude-harness session without the picker |
| `--resume-mode <auto\|native\|portable>` | How `-c`/`--last` resumes: `native` is Claude's own resume, `portable` starts a new session from a compact summary plus the newest turns that fit, `auto` (default) picks native when safe |
| `--coauthor[=TAG]` | Opt in to commit/PR attribution. clud hides Claude Code's `Co-Authored-By` trailer and "Generated with Claude Code" PR line by default; bare `--coauthor` keeps Claude's own, `=TAG` uses TAG for both (also `CLUD_COAUTHOR=1|TAG`; Claude harness only) |
| `--dry-run` | Print what would be executed, then exit |
| `-v`, `--verbose` | Show debug output |
| `-h`, `--help` | Show help |
| `-V`, `--version` | Show version |

Unknown flags are forwarded directly to the backend agent.

### Local API gateway

`clud serve --claude` or `clud serve --codex` exposes a local
Anthropic-compatible API for other clients.
Add `--openrouter` to make configured OpenRouter models available. See the
[server setup and examples](docs/architecture/standalone-gateway.md), or
[Hermes integration](#hermes-integration) for MCP discovery.

### OpenRouter through Claude Code

OpenRouter uses its own API key; a DeepSeek or Anthropic key cannot authenticate
there. Store it in the native credential vault, then launch:

```bash
clud auth login openrouter
clud --openrouter
```

Clud connects Claude Code directly to `https://openrouter.ai/api`, explicitly
clears inherited Anthropic API-key auth, and enables gateway model discovery.
OpenRouter is the routing gateway; Claude Code remains the interactive frontend.

Choose the main model in one of three ways:

```bash
# Use clud's reviewed default: OpenRouter's latest Claude Sonnet alias
clud --openrouter

# Pin a documented OpenRouter alias before launch
clud --openrouter --model "~anthropic/claude-opus-latest"
clud --openrouter --model "~anthropic/claude-haiku-latest"

# Or pass an exact model slug from OpenRouter's catalog
clud --openrouter --model "anthropic/<model-slug>"
```

After launch, run `/model` inside Claude Code to use the live model inventory
discovered from OpenRouter. Clud does not currently show its own OpenRouter
picker before Claude Code starts; use `--model` for deterministic startup or
Claude Code's `/model` picker for interactive selection. `--model` controls the
main model, while Claude Code's Fable, Opus, Sonnet, Haiku, and subagent roles
retain OpenRouter's documented role aliases.

Arbitrary OpenRouter model slugs are accepted, but non-Claude models are
best-effort because the Claude harness is only guaranteed along OpenRouter's
Anthropic-first-party Claude path. Without an OpenRouter key, `--dry-run` can
verify routing and selection but live discovery and requests cannot run:

```bash
clud --openrouter --model "~anthropic/claude-opus-latest" --dry-run
```

If Claude Code has a cached Anthropic login and reports authentication or
model-not-found errors, run `/logout` once inside Claude Code, exit, and
relaunch `clud --openrouter`. Use `clud --claude` to return to native Claude
routing.

From a real terminal, every harness launches in a PTY; with redirected output
or a headless run (`-p`, `codex exec`) it launches as a subprocess instead.
`--pty` and `--subprocess` override that choice. See
[DD-086](docs/DESIGN_DECISIONS.md#dd-086-every-console-launch-runs-through-the-pty-pump-subprocess-is-for-headless-and-redirected-output).

## PTY Graphics Headers

PTY sessions can reserve a small header area above the backend terminal and
draw a Sixel image there. `--graphics=auto` is conservative: it only enables
the header when `running-process` reports Sixel as supported from a live probe
of the current terminal. Missing metadata, non-TTY attaches, blocked terminals,
and host-name-only hints stay text-only. Use `--graphics=off` to disable the
feature or `--graphics=sixel` to force it.

By default clud renders the bundled README hero image. Pass
`--graphics-image <PATH>` to use a PNG or JPEG instead. Direct PTY launches
reserve rows before the backend starts. Daemon-managed sessions decide at attach
time, because the
detached worker does not know which terminal will attach later; reattach and
resize paths redraw the header where the terminal reports support.

## Codex Support

![codex-supported](https://github.com/user-attachments/assets/de1e23b4-4513-4c92-ba57-3d9dcd1060b6)

The Rust version of `clud` supports Codex directly. Use `--codex` to switch
backends for interactive runs, prompt-driven execution, resume flows, and
detachable sessions. With no explicit or saved Codex profile, direct Codex
launches start on Sol at low effort. `--model`, `--effort`, and saved
`providers.codex` settings override that reviewed fallback.

### Codex through Claude Code (experimental)

To use a Codex model with Claude Code's harness features, opt in explicitly:

```bash
clud --codex --harness claude
clud --codex --harness claude --model terra@high
```

Claude Code 2.1.223 or newer shows separate Codex Sol, Terra, and Luna rows in
`/model`. Clud advertises reserved discovery IDs to the harness, rewrites them
to the real OpenAI model IDs inside the launch-scoped bridge, and carries
reasoning effort independently. Older Claude Code clients fail at launch with
an update instruction instead of silently showing a degraded picker.

Provider and harness are separate choices. `--harness default` restores the
provider's native harness for one launch. An explicit session/global choice is
offered interactively; global choices are stored in `~/.clud/settings.json`.
When a saved non-default harness is used on a TTY, clud prints a green
`[clud] Harness override: Claude (global setting)` notice. CLI flags always
override saved settings.

Use a platform API key by setting `OPENAI_API_KEY` in the launch environment.
ChatGPT subscription login is experimental and compatibility-sensitive:

```bash
clud codex-auth login --acknowledge-experimental
clud codex-auth status
clud codex-auth logout
```

The subscription record is clud-owned and never falls back silently to an API
key. `logout` removes only clud's record, not Codex CLI credentials.

#### Cross-route troubleshooting

- **Claude executable missing:** install Claude Code and ensure `claude` is on
  PATH. Use `clud --codex --harness default` while fixing the installation.
- **Unsupported pair:** only Codex provider through Claude is supported. Claude
  provider through Codex is rejected before launch.
- **Login expired:** run `clud codex-auth login --acknowledge-experimental`;
  do not expect an existing subscription record to fall back to an API key.
- **Callback ports occupied:** free 1455 or 1457, then retry login. The command
  uses 1455 first and 1457 only as its fallback.
- **Bridge start or upstream failure:** run `clud --dry-run --codex --harness
  claude` to inspect the resolved target. Check proxy/firewall rules permit the
  configured OpenAI endpoint; upstream 4xx errors usually require a credential,
  model, or request change, while transient 5xx/429 failures are retried only
  before output begins.
- **Disable/rollback:** pass `--harness default` or reset the stored harness in
  `clud settings`. Native `clud`, `clud --claude`, and `clud --codex` launches
  are unaffected.

Compatibility evidence, security boundaries, and the no-sidecar design live in
[the Codex-via-Claude architecture document](docs/architecture/codex-via-claude.md).

### Codex Hook Warnings

On `clud --codex` launches, clud runs a lightweight hook-health check before
starting Codex. The check compares Claude Code and Codex `PreToolUse` hook
coverage and inspects Codex hook trust state. These warnings are informational;
normal launch continues unless the backend itself fails.

If Codex has `PreToolUse` hooks but Claude Code does not, clud prints:

```text
[clud] warning: Codex PreToolUse hooks exist, but Claude PreToolUse hooks are missing or inactive. Run `clud --fix-hooks`.
```

Run `clud --dry-run --fix-hooks` to see the planned repair actions. Run
`clud --fix-hooks` only when you want clud to add deterministic Codex trust
entries or ask the selected backend to translate a missing hook between Claude
Code and Codex.

Codex hook matchers may use `*` as a catch-all. When equivalent Claude Code
hooks use the same command across several tool matchers, the Codex repair plan
prefers one catch-all hook instead of repeated per-tool prompts. That reduces
duplicate Codex hook review/trust approvals while keeping per-tool hooks when
commands differ.

On Windows, Codex hook commands that call a `.cmd` or `.bat` wrapper need
explicit exit-code propagation. If the command does not include
`$LASTEXITCODE`, clud warns:

```text
[clud] warning: Codex hook command in C:\Users\you\.codex\hooks.json uses a Windows batch wrapper without explicit `$LASTEXITCODE` propagation; a blocking hook may fail open.
```

Fix the hook by invoking a native executable directly, or by making the
PowerShell hook command end with `exit $LASTEXITCODE` after the batch wrapper.
Without that, a hook intended to block a tool call can return success to Codex
after the wrapper fails.

## `clud-cmd-scan` — Command Guard

Agents sometimes reach for the wrong command — bare `cargo` when your repo
needs `soldr cargo`, or `playwright` when you have a faster test script.
`clud-cmd-scan` catches those *before* they run. It's a hook that reads every
shell command the agent is about to execute, and for the ones you've banned it
blocks the command and tells the agent what to run instead.

clud ships with a few built-in rules (bare `cargo`/`rustc` → `soldr cargo`,
whole-disk `find /`). You add your own in `.clud/settings.json`.

### Turn it on

The guard runs as a Claude Code hook. Add it once to `~/.claude/settings.json`
(covers every repo) or to a single repo's `.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "*",
        "hooks": [{ "type": "command", "command": "clud-cmd-scan", "timeout": 30 }]
      }
    ]
  }
}
```

Use `"matcher": "*"`, not `"Bash"`. A `Bash`-scoped matcher only guards the
Bash tool, so catastrophic commands (`rm -rf $VAR/`) issued through Claude
Code's PowerShell tool, an MCP shell server (`mcp__*`), or a Codex session slip
past unchecked (zackees/clud#1084). `"*"` scans every tool call; the hook
self-selects the shell dialect internally, so a non-shell tool call is a cheap
no-op.

If you ever see `clud-cmd-scan: command not found`, the guard isn't installed
and is doing nothing — run `uv tool install --force clud`. (If you have an old
config that calls `block-bad-cmd`, clud quietly upgrades it to `clud-cmd-scan`
the next time it launches.)

### Write your first rule

A rule says: *when the agent runs command X, block it and suggest Y.* Put it in
`bad_commands`:

```json
{
  "bad_commands": [
    {
      "id": "no-raw-playwright",
      "match": "playwright",
      "replacement": "npm run test:integration",
      "reason": "use the blessed pipeline; raw playwright is slower.",
      "allow_override": true
    }
  ]
}
```

Now when the agent tries to run `playwright`, it gets stopped:

```text
$ playwright test
use the blessed pipeline; raw playwright is slower. Use `npm run test:integration` instead.
Blocked `playwright` by rule `no-raw-playwright`
from `/repo/.clud/settings.json#/bad_commands/0`.
```

The block message always names the exact rule and file that fired, so you're
never left guessing why something was denied.

**One thing to know about `match`:** it only looks at the *program being run* —
the first word of the command — not the whole line. So a rule for `playwright`
stops `playwright test`, but leaves `rg playwright` and `grep playwright .`
alone, because there you're *searching for* the word, not running the program.
By default `match` is a glob (`*`, `?`), matching the whole program name; add
`"match_mode": "regex"` if you need a regex.

You don't have to worry about the agent sneaking a command past the rule with
`&&`, pipes, `bash -c '...'`, `$(...)`, `eval`, and so on — the scanner unwraps
all of those and checks inside. Words merely quoted in an `echo` are left alone.

### Where rules live

You can define rules in three files. They **add together** — a rule in any of
them applies:

| File | Use it for |
| --- | --- |
| `~/.clud/settings.json` | rules you want on your own machine, everywhere |
| `<repo>/.clud/settings.json` | rules for the whole team — commit it |
| `<repo>/.clud/settings.local.json` | your personal rules for one repo — gitignored |

If two rules share an `id`, the more specific file wins (repo over user).

### Block only the *dangerous* form of a command

Sometimes `git push` is fine but `git push --force` isn't. Add an `arguments`
block to match on the flags:

```json
{
  "bad_commands": [
    {
      "id": "no-force-push",
      "match": "git",
      "arguments": {
        "ordered": ["push"],
        "any": ["--force", "-f"],
        "none": ["--force-with-lease"]
      },
      "replacement": "git push --force-with-lease",
      "reason": "unconditional force pushes can overwrite remote work."
    }
  ]
}
```

This blocks `git push --force` but lets `git push --force-with-lease` and
`git status` through. The building blocks: `any` (at least one is present),
`all` (all present), `none` (none present), and `ordered` (these appear in this
order, gaps allowed). Everything you list must hold at once. See DD-017 for the
rest (`prefix`, `contiguous`, short-flag bundles like `-rf`, and looking past
wrappers like `sudo`).

### Block a *pipe* between two commands

Some risks are about two commands together — like piping a download straight
into a shell. Those go in `bad_pipelines`:

```json
{
  "bad_pipelines": [
    {
      "id": "no-download-to-shell",
      "stages": [
        { "match": "curl" },
        { "match": "^(?:ba)?sh$", "match_mode": "regex" }
      ],
      "replacement": "download the script, inspect it, then run it",
      "reason": "piping downloaded content into a shell hides executed code."
    }
  ]
}
```

This trips on `curl ... | sh` (and `| bash`). A `|` inside a quoted string
doesn't count as a real pipe.

### Let the agent through, just this once

If a rule sets `"allow_override": true`, you can wave a single command past it
by setting an environment variable — with a reason:

```bash
CLUD_BAD_CMD_OVERRIDE="no-raw-playwright:debugging a trace-viewer bug"
```

The `id:reason` form is required; an override with no reason is ignored and the
command stays blocked. It has to be a real environment variable — you can't
just type it in front of the command.

### Turn the guard off entirely

`CLUD_ALLOW_ALL_CMDS=1` skips every `clud-cmd-scan` check, built-in ones
included (the rm-shim identity check, the `cd` pin, `bad_commands`,
`bad_pipelines`, and the `/grind` role caps). It is the escape hatch for when
the guard itself is wedged — for example, a rebuilt `clud` that no longer
matches the installed rm shim, which makes the guard deny every shell call.

```bash
export CLUD_ALLOW_ALL_CMDS=1        # the whole session
```

Unlike `CLUD_BAD_CMD_OVERRIDE`, it also works written into a single command
(`export CLUD_ALLOW_ALL_CMDS=1; <cmd>` or `CLUD_ALLOW_ALL_CMDS=1 <cmd>`), so an
agent can repair a wedged guard. The one exception: `/grind`'s capped
`grind-*` agents cannot lift their own caps this way. Only an explicit `1`
counts.

### Skip only the rm shim identity check

`CLUD_SKIP_RM_IDENTITY=1` in the hook's environment skips just the rm-shim
identity check; every other check, including the `/grind` role caps, still
runs. clud sets it automatically when launched inside clud's own repo
(detected by `crates/clud-bin/Cargo.toml` naming the `clud` package) and
prints a yellow startup warning, because `bash build` replaces
`target/debug/clud` and would otherwise wedge every shell call. Unlike
`CLUD_ALLOW_ALL_CMDS`, writing it into a command has no effect.

### Keep the agent from wandering out of the repo

A `cd` in a Bash tool call moves the **session** cwd — not just that one
command's. Every later tool call inherits it, and anything resolving a relative
path against it breaks at once. Project hooks are the usual casualty: they are
conventionally written as repo-relative script paths, so one stray `cd` can
wedge a session badly enough that no tool runs.

`bash.block_cd` pins the session cwd:

```json
{
  "bash": {
    "block_cd": "auto"
  }
}
```

| Value | Meaning |
| --- | --- |
| `"auto"` (default) | decide per repo, from the hooks actually in scope |
| `true` | always pin: the session cwd must *be* the repo root |
| `false` | never police `cd` |

`"auto"` looks at every hook configured for the repo — all events, both
frontends. A hook that runs a relative script path (`uv run python
ci/hooks/check.py`) breaks on *any* drift, so `"auto"` pins the cwd to the repo
root. A repo whose hooks are all PATH binaries or absolute paths only needs the
weaker rule, so `cd src/` stays allowed and only leaving the repo is blocked. A
repo with no hooks at all is left alone entirely.

A `cd` that cannot move the session is never blocked, so the workaround is
always available:

```bash
(cd build && cmake --build .)    # subshell — the session never moves
git -C vendor/lib log -1         # tool-level directory flag
bash -c 'cd build && make'       # child process
```

`cd` back to the repo root stays allowed under every setting — that is how a
session recovers if the cwd has already drifted. To move for one call anyway,
`CLUD_BAD_CMD_OVERRIDE="block-cd:<reason>"` works exactly as it does for
`bad_commands` rules.

If clud tells you a hook is cwd-sensitive, the one-line fix is to root it at
the session's project directory instead of hoping the cwd is right:

```bash
cd "$CLAUDE_PROJECT_DIR" && uv run python ci/hooks/check.py
```

clud also warns at launch — and in `clud --fix-hooks` — about the
widely-copied `git rev-parse --show-superproject-working-tree || ...` prefix,
which looks like it does this but does not: that flag exits 0 with empty output
outside a submodule, so the `||` fallback never runs and `cd ""` silently does
nothing.

### Stop a subagent from spawning more agents

One request can become hundreds of agents when a subagent delegates again: a
deep-research run created 101 agents before it was stopped, and an upstream
`/code-review` created 877 descendants *despite* a five-level nesting ceiling.
A depth ceiling does not help, because fan-out at every level is what makes the
total exponential.

`clud-cmd-scan` can refuse the second level outright. With the guard on, the
primary session may still create subagents, but a subagent's own `Agent` or
`Workflow` call is denied before the descendant is allocated:

```bash
export CLUD_BLOCK_RECURSIVE_AGENTS=1
```

The denial is tagged `clud_agent_depth_exceeded` and tells the agent to do the
work itself or hand it back to the session that started it. The guard is off by
default — refusing agent creation changes what an agent may do, and a wrong
default would break legitimate orchestration silently.

Two things to know:

- The hook must be registered with `"matcher": "*"` (the config above), not
  `"Bash"`. A `Bash`-only matcher never sees an `Agent` call.
- Only tool-visible creation is covered. The `agent()` calls a workflow script
  makes internally emit no blocking hook event, so they remain uncontrolled
  (anthropics/claude-code#79953 tracks the upstream gap).

### Let clud run your project's hooks

Project hooks are usually written as repo-relative script paths, which is why a
stray `cd` breaks them: the hook resolves its path against wherever the agent
ended up. You can hand those hooks to clud instead, and it will always run them
from your repo root.

Declare them in `.clud/hooks.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "command": "uv run python ci/hooks/check_cmd.py" }
    ],
    "Stop": [
      { "command": "uv run python ci/hooks/check_on_stop.py", "timeout": 120 }
    ]
  }
}
```

Every hook declared here runs with:

- **the working directory set to your repo root**, whatever the session cwd is
- **`CLUD_PROJECT_DIR`** set to that same root, on every backend
- the hook payload on stdin, exactly as the harness would have sent it

`matcher` is a regex over the tool name and is anchored, so `Edit` matches
`Edit` and not `MultiEdit`; leave it out to match every tool. `timeout` is in
seconds and defaults to 60.

Exit codes work the way you already expect: **0** carries on, **2** blocks the
call and shows the model whatever your hook printed. Anything else — a crash, a
missing interpreter, a timeout — is reported as a warning and the call
proceeds. A guard that can't run is a bug in the guard, and turning it into a
wall in front of every tool call is exactly the wedge this feature exists to
prevent.

**Move hooks here, don't copy them.** A command left in `.claude/settings.json`
*and* declared in `.clud/hooks.json` runs twice, and the copy the harness fires
is the one that isn't rooted — the failure you were trying to fix. clud warns
when it sees the same command in both places.

### What it is (and isn't)

This is a guardrail for a *cooperative* agent, not a security wall. If it can't
parse a command it lets it through (fails open), and it makes no attempt to
stop someone deliberately hiding a command — via a shell variable, base64, a
throwaway script, etc. Its whole job is to stop an agent from *accidentally*
grabbing the wrong tool.

For the complete field reference, see **DD-016** and **DD-017** in
[`docs/DESIGN_DECISIONS.md`](docs/DESIGN_DECISIONS.md); **DD-047** covers
`bash.block_cd`.

## Detached Sessions

Use daemon-managed sessions when you want to disconnect and reattach later.

```bash
clud --detachable --codex -p "refactor the parser"
# press Ctrl+C, then press y within 5 seconds to keep it running in background

clud attach
clud attach sess-123
clud list
```

If you press `Ctrl+C` in a `--detachable` session, clud asks `continue session in
the background?` with a 5-second countdown. Press `y` to background it. Press
`Ctrl+C` again, press anything else, or do nothing to end the session instead.

`clud attach` without a session ID lists background sessions. `clud list` shows
the same sessions with their root PID and current working directory.

### Daemon idle lifetime

The daemon starts on demand and, by default, exits after 15 minutes with no active work. This
releases its GC database and background resources; the next normal daemon-backed command starts
it again transparently. Active foreground clients, detached/repeat sessions, dashboard or top
polling, RPC connections, and maintenance prevent this shutdown. Configure
`daemon.idle_timeout_secs` in `~/.clud/settings.json` to another positive number of seconds, or
set it to `0` to disable idle retirement.

## Voice Mode (F3 push-to-talk)

`clud` captures microphone input and transcribes it directly into the active
backend prompt using local `whisper.cpp`. Hold `F3`, speak, release `F3`, and
the transcript appears at your cursor without auto-submitting — you can edit
it before pressing Enter. Available on **all six** supported platforms
(Linux x86/ARM, Windows x86/ARM, macOS x86/ARM). On Linux, microphone capture
uses `arecord` on demand so `libasound` is not required for normal startup.

### Enabling it

The minimum is a single env var:

```bash
export CLUD_VOICE=1
clud
```

```powershell
# Windows PowerShell
$env:CLUD_VOICE = "1"
clud
```

On first F3 press, `clud` auto-downloads the Whisper `ggml-small.en.bin`
model (~466 MB) into a per-OS cache directory and verifies it against a
pinned SHA-256. The download runs in the background as soon as voice mode
starts up, so by the time you reach for `F3` it's usually ready.

| Platform | Cache path |
|----------|-----------|
| Linux | `~/.cache/clud/whisper/ggml-small.en.bin` |
| macOS | `~/Library/Caches/clud/whisper/ggml-small.en.bin` |
| Windows | `%LOCALAPPDATA%\clud\whisper\ggml-small.en.bin` |

If you already have a model on disk, point `CLUD_WHISPER_MODEL` at it and
the auto-download is skipped.

### How `F3` behaves on different terminals

| Terminal | Behavior |
|----------|----------|
| Kitty-protocol terminals (kitty, Ghostty, modern iTerm2, WezTerm, Alacritty with kitty mode) | True press-and-hold: recording stops the instant you release `F3`. |
| Everything else (Windows Terminal / ConPTY, older xterm, etc.) | Press `F3` to start; recording auto-stops after 1.5 seconds of silence (VAD) or 30 seconds maximum, whichever comes first. |

Cues are short tones generated programmatically on macOS/Windows — `ding`
on start (~880 Hz, 90 ms), `dong` on stop (~660 Hz, 120 ms). Linux uses a
terminal bell so `clud` does not link audio output libraries at startup. If
the default audio output device is unavailable, `clud` falls back to a
terminal bell.

### Environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `CLUD_VOICE` | unset | Enable voice mode (`1`, `true`, `yes`, `on`). Setting `CLUD_WHISPER_MODEL` also implicitly enables it. |
| `CLUD_WHISPER_MODEL` | auto-managed cache path | Override the model location. Trusted as-is — no hash check on user paths. |
| `CLUD_VOICE_LANGUAGE` | inferred (English with `small.en`) | Force a Whisper language code, e.g. `en`, `de`, `fr`. |
| `CLUD_VOICE_TEST_TRANSCRIPT` | unset | Test-only bypass: replaces real transcription with this exact string. Used by the integration test suite. |

### Troubleshooting

- **Nothing happens when I press F3.** Check that `CLUD_VOICE=1` is exported in the same shell. On Linux, install `alsa-utils` so `arecord` is available, then verify a default input device exists (`arecord -l` on Linux, "Sound" preferences on macOS/Windows).
- **"voice mode is enabled but the Whisper model is not yet available"** — the auto-download hasn't finished. Watch stderr for `[clud] voice: download N% (...)` lines, or pre-seed the cache path manually with `curl -L -o <cache-path> https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin`.
- **Recording keeps stopping mid-sentence on non-kitty terminals.** The VAD silence window is 1.5 s — pause less, or switch to a kitty-protocol terminal for true hold-to-record.
- **Transcript is empty / garbage.** Whisper struggles on very short utterances and noisy backgrounds. The `MIN_CAPTURE_MS` floor (150 ms) silently drops sub-150 ms blips; speak for at least half a second.

## `clud loop` — The Ralph Loop

![clud-loop-ralph](https://github.com/user-attachments/assets/b6666429-ead7-419c-831f-db4e17b3840b)

Run the backend in a **ralph loop**: iterate on a task until the agent signals
it's done, or until the iteration count runs out. Fully autonomous — no user
interaction between iterations.

```bash
clud loop "Implement the API endpoints from the spec"
clud loop TASK.md                                  # Read prompt from a file
clud loop https://github.com/org/repo/issues/42    # Fetch & iterate on a GH issue
clud loop --loop-count 10 "fix bugs"               # Custom iteration count
```

### In-chat `/loop` for Codex models

Codex does not ship a `/loop` command. clud used to fill that gap with a bundled
`clud-loop` skill; it is **retired** as of the `--harness claude` cross-route.
Run Codex models under the Claude harness and you get the harness's own `/loop`:

```bash
clud --codex --harness claude
```

Retiring it also fixes a mis-fire: the skill's triggers keyed on the word
"Codex", so a Codex model driving the *Claude* harness would pick the polyfill
over the harness's native `/loop`. clud purges the installed copy from both
`~/.claude/skills/` and `~/.codex/skills/` on next launch, leaving any copy you
edited yourself in place.

For a plain `clud --codex` session with no cross-route, the external runner is
still there:

```bash
clud --codex loop .clud/loop/LOOP.md
clud --codex loop --repeat 30m --loop-count 1 --no-done .clud/loop/LOOP.md
```

### Task input modes

The positional argument is classified in this order:

1. **GH issue / PR URL** — the issue body is fetched via `gh` and cached to
   `<git-root>/.clud/loop/<owner>__<repo>__issue-<n>.md`. Subsequent runs
   reuse the cache; pass `--refresh` to force a re-fetch.
2. **Short form `#42`** — resolves `owner/repo` via `gh repo view`.
3. **Local file path** — read as the prompt.
4. **Literal string** — used as-is.

### Completion signal (DONE / BLOCKED marker files)

`clud loop` injects a short contract into the prompt asking the agent to write
one of two marker files under `<git-root>/.clud/loop/`:

| Marker    | Meaning                                    | Exit code |
|-----------|--------------------------------------------|-----------|
| `DONE`    | Task resolved (one-line summary inside)    | 0         |
| `BLOCKED` | Agent can't proceed (reason inside)        | 3         |
| (neither) | Iteration count exhausted                  | 2         |
| non-zero backend exit | Infra failure                  | propagate |

Stale `DONE` / `BLOCKED` files from a prior run are cleared at start so the
loop can't short-circuit on iteration 1.

Opt out with `--no-done-marker` to restore the old "run N times unless the
backend fails" behavior.

## `clud rebase` — Auto-Rebase

Fetches from origin, rebases the current branch, and resolves conflicts.

```bash
clud rebase
```

## `clud fix` — Auto-Fix

Detects linting and test tools in your repo, runs them, and fixes failures in a loop until everything passes.

```bash
clud fix
```

## `clud grind` — Burn Down Issues to Merged PRs

```bash
clud grind                                          # this repo's open issues
clud grind https://github.com/<owner>/<repo>/issues/<N>  # a meta issue's sub-issues
clud --codex --harness claude grind                 # a Codex model on the Claude harness
```

`clud grind [url]` opens one interactive Claude session seeded with `/grind`.
Inside a session you can also type `/grind <meta issue | issue list | goal>`
directly. Before anything runs, `/grind` asks:

| Question | Choices |
|---|---|
| Mode | **parallel**: a git worktree per goal; server-side CI runs concurrently. **sequential**: one goal at a time in the local checkout, with no worktrees, so a heavy C++/Rust build cache stays warm. **cron**: one issue per native `/loop` tick. |
| Models | planner, worker, reviewer, integrator. Each defaults to the session's model, including whatever `--deepseek`, `--codex` or OpenRouter resolved. |
| Local CI | run the `ci.yml` job under `act` before each push. Only offered when Docker works and `.github/workflows/ci.yml` exists. |

Before any goal starts, a `grind-prework` agent posts the run's plan as one
JSON comment on the meta issue (it may only comment there; nobody edits that
comment), and every later agent is given its URL. If the plan cannot be
posted, no work happens.

Then the bundled workflow takes each goal through five roles. Each role is its
own agent type with hard caps:

| Stage | Agent type | May | May not |
|---|---|---|---|
| Plan | `grind-planner` | read code; read-only `git`/`gh`; add a worktree in parallel mode | edit files, build |
| Work | `grind-worker` | edit its assigned files; read-only `gh`; web search | build, lint, test |
| Review | `grind-reviewer` | same as worker | same as worker |
| Integrate | `grind-integrator` | rebase, lint, build, test, push, open the PR; **one at a time** | `bosn`, direct `docker`, new worktrees |
| Land | `grind-lander` | watch with `pr_merge_watch`, admin-merge when green | edit, build; failures go back to the integrator (≤10 rounds) |

At most four plan/work/review agents run at once, and only the integrator
builds, so builds never overlap. A goal that depends on another lands after
it. The run ends with the checkout rebased onto `origin/main` and a clean
`git status`. `grind` requires the Claude harness; clud never runs its own
loop for it.

Problems an agent finds outside its task (a flaky test, an unrelated bug)
are returned to `/grind`, which files them as you chose up front: one
`grind:followup` issue each (linked with `Refs #<meta>`, never a sub-issue,
so it never holds the meta issue open), or one comment each.

**Where it lives in the repo:**

| What | Where |
|---|---|
| Contract and design | [`docs/architecture/grind.md`](docs/architecture/grind.md), [DD-087](docs/DESIGN_DECISIONS.md#dd-087-grind-is-a-skill-dag-with-capped-agent-roles) |
| Router skill | [`assets/skills/grind/SKILL.md`](crates/clud-bin/assets/skills/grind/SKILL.md) |
| Leaf skills | [`grind-intake`](crates/clud-bin/assets/skills/grind-intake/SKILL.md), [`grind-prework`](crates/clud-bin/assets/skills/grind-prework/SKILL.md), [`grind-plan`](crates/clud-bin/assets/skills/grind-plan/SKILL.md), [`grind-work`](crates/clud-bin/assets/skills/grind-work/SKILL.md), [`grind-review`](crates/clud-bin/assets/skills/grind-review/SKILL.md), [`grind-integrate`](crates/clud-bin/assets/skills/grind-integrate/SKILL.md), [`grind-land`](crates/clud-bin/assets/skills/grind-land/SKILL.md), [`grind-cron`](crates/clud-bin/assets/skills/grind-cron/SKILL.md) |
| Workflow script | [`assets/workflows/grind-run.js`](crates/clud-bin/assets/workflows/grind-run.js) (named `grind-run` so `/grind` stays the router) |
| Agent types | [`assets/agents/`](crates/clud-bin/assets/agents/) (`grind-planner.md` … `grind-lander.md`) |
| Installer for agents + workflow | [`src/claude_files.rs`](crates/clud-bin/src/claude_files.rs) |
| Per-role shell caps (hook) | [`src/block_bad_cmd_grind_caps.rs`](crates/clud-bin/src/block_bad_cmd_grind_caps.rs) |
| `clud grind` launch + prompt | [`src/command/prompts.rs`](crates/clud-bin/src/command/prompts.rs) (`GRIND_TEMPLATE`), [`src/command/builder.rs`](crates/clud-bin/src/command/builder.rs) (`grind_launch_error`), [`src/grind.rs`](crates/clud-bin/src/grind.rs) (issues-URL resolution) |
| PR watcher used by the lander | [`assets/tools/github/pr_merge_watch.py`](crates/clud-bin/assets/tools/github/pr_merge_watch.py) |

Only `/grind` is offered to the model. The `grind-*` leaf skills are hidden
from it, though you can still type one (e.g. `/grind-work`) to test it. The
`grind-*` agent types are refused when the model tries to delegate to them
directly; set `CLUD_ALLOW_GRIND_AGENTS=1` to start one by hand.

`CLUD_ALLOW_ALL_CMDS=1` in the environment turns clud's command hook off,
including these caps.

## `clud do [url-or-goal]` — Implement to a Merged PR

Launches an interactive agent session seeded with `/goal <rendered procedure>`.
`/goal` keeps the session going until the bundled
[`/do`](crates/clud-bin/assets/skills/do/SKILL.md) contract is met: the work lands
in merged PRs, referenced issues are closed, and the checkout is clean and
rebased. A single issue URL, a PR, or a free-form goal all get the `/do` contract.

`clud do` renders the procedure before launching the harness. Typing
`/do <target>` in Claude Code runs `clud do-prompt`, using the same renderer.
Both check the current branch first:
- On the default branch, it starts there.
- On a branch with an open PR, or one named for the issue, it continues there.
- On a branch whose PR merged, or that has no commits, it tells the agent to
  switch to `main` first.
- With uncommitted changes or an unrelated branch, the agent asks you before
  doing anything.

A GitHub issue with open sub-issues is a meta issue. clud checks this with
`gh` before launch and includes an explicit handoff to read the installed
[`grind` router](docs/architecture/grind.md), so the children are worked through
its workflow. Meta launches require the Claude harness and a foreground
interactive session; a missing Workflow tool or bundled agents stops the handoff.
Neither launch relies on nested slash-command expansion or model-side Skill
invocation; do/grind invocation policies remain explicit-only.
If clud can't tell (no
`gh`, not signed in, offline, or a non-GitHub issue URL), it refuses to launch;
set `CLUD_DO_KIND=single` or `CLUD_DO_KIND=meta` to choose yourself.
With no target, an interactive foreground launch prompts for a URL or goal before
starting the backend. Scripts, dry-runs, and background launches must provide the
target explicitly so they never block on input.

```bash
clud do https://github.com/zackees/clud/issues/866
clud --codex do "refactor the launch mode classifier"
clud --codex do                 # prompts: Enter an issue URL or goal
clud do --dry-run https://github.com/zackees/clud/issues/866
```

On the native Codex harness, the built-ins `do`, `up`, `rebase`, and `fix`
seed the interactive TUI rather than `codex exec`, so progress and follow-up
input stay live. They can resume `--continue` or `--resume=<session>`; bare
`--resume` is rejected because Codex would mistake the generated prompt for a
session ID. Explicit `-p` and `loop` runs remain non-interactive.

## `clud up` — Ship It

Runs lint, test, cleanup, then commits.

```bash
clud up
```

## `clud wasm` — Embedded Runtime

Loads a local `.wasm` module, wires up a host logging import, and invokes an exported function.

```bash
clud wasm hello.wasm
clud wasm hello.wasm --invoke _start
```

## Development

```bash
bash build                  # Build dev wheel (Rust binary + Python package)
bash lint                   # Lint (cargo fmt + clippy + ruff + banned imports)
bash test                   # Unit tests (Rust + Python)
bash test --integration     # Include integration tests with mock agents
```


## License

Clud Proprietary License. Free use is available for individuals and
organizations under 6 people, with lifetime grandfathering for organizations
that qualified before growing beyond that size. Larger organizations need a
commercial license unless they have a grandfathered or contributor-granted free
license. See [LICENSE](LICENSE) for the full terms.

# Secret catalog (`--allow-secrets`)

`clud --allow-secrets` tells an agent which credentials exist on the machine and
where they are, without giving it their values. It is opt-in per launch; without
the flag, none of this runs. It exists because an agent that can't see a
credential will report it as missing: FastLED's issue sanitizer held every
outside issue for lack of an `OPENROUTER_API_KEY` secret while a valid key sat in
another program's environment on the same machine.

## What a launch adds

On the Claude backend, `command/builder.rs` adds two things to the launch:

- `--mcp-config=<json>`, which registers the `lokr` MCP server as the hidden
  `clud secrets-mcp` subcommand.
- `--append-system-prompt`, carrying `secrets_catalog::DIRECTIVE`, which names the
  routes and tells the agent to check before concluding that a key is missing.

The server starts the hidden `clud secrets-daemon` subcommand. It rescans every 60
seconds on a background thread, writes names and locations to
`~/.lokr/catalog.json` (mode 0600), and exits after 10 minutes without an MCP
heartbeat.

## Routes

| Route | Result |
|---|---|
| `mcp__lokr__secret_status` | Whether a named key exists, and where |
| `mcp__lokr__list_secrets` | Every known credential, by name and location |
| `mcp__lokr__request_secret` | Opens the masked `clud secret` window, writes to `~/.lokr/store/<name>`, and returns only `stored`, `failed` or `cancelled` |

No route returns a value, and there must never be a `get_secret` route. The scan
reads values only to skip empty ones. `request_secret` hands the value straight
from the prompt to its sink; see [secret-prompt.md](secret-prompt.md).

## What gets scanned

- The environment.
- Environments of same-user processes (Linux only).
- `.env` files: `~/.env`, `~/.<tool>/.env` and `~/.config/<tool>/{.env,*.env}`.
- Known credential files, such as `~/.cargo/credentials.toml` and
  `~/.aws/credentials`.
- Known keyring entries. Keyrings can't be enumerated, so other entries aren't
  found.

## How an agent should use it

1. Call `secret_status` before saying a key is missing.
2. Ask the user before reusing a key held by another program. Seeing that a key
   exists doesn't authorize using it.
3. If the user approves, send the value over stdin to its destination. Never put
   it in chat, command arguments, printed variables or logs.
4. If the key is absent, call `request_secret`.

## Limits

- This prevents accidental exposure; it is not a sandbox. An agent with shell
  access under the same account can still read these locations.
- The flag is silently ignored on the Codex backend.
- `request_secret`'s prompt window has not been exercised end to end.
- lokr is pinned by git commit in `crates/clud-bin/Cargo.toml` until it has a real
  crates.io release.

The scanner and MCP server live in the [lokr](https://github.com/zackees/lokr)
library; its `DESIGN.md` owns their internals. clud's integration is
`crates/clud-bin/src/secrets_catalog.rs`.

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

`clud secrets-mcp` runs the resolved `lokr mcp` executable with inherited stdio.
The lokr server starts its own daemon. It rescans every 60
seconds on a background thread, writes names and locations to
`~/.lokr/catalog.json` (mode 0600), and exits after 10 minutes without an MCP
heartbeat.

## Routes

| Route | Result |
|---|---|
| `mcp__lokr__secret_status` | Whether a named key exists, and where |
| `mcp__lokr__list_secrets` | Every known credential, by name and location |
| `mcp__lokr__request_secret` | Requires title/message and an explicit file, keyring or command sink; uses `lokr request` and returns only `stored`, `failed` or `cancelled` |

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

## sudo

`clud sudo -- <command>` is the one way an agent clud launches gets a sudo
password. Three places carry the contract to every session:

- the `clud-secret` skill, which loads automatically and leads with the recipe;
- one sentence of `TOOL_CALL_RULES` in the appended system prompt, for
  Claude and Codex (`command/builder.rs`);
- the `sudo-prompt` hook rule (`block_bad_cmd_sudo.rs`), which refuses any
  `sudo` invocation that would prompt and names the exact `clud sudo` command.

**Why one call per command.** An agent's shell call runs in a new session with
no controlling terminal. Without a tty, sudo keys a ticket on the parent
process: `timestamp_type=tty` falls back to `ppid`. Container runs on Debian
12 (sudo 1.9.13), Fedora 41 (1.9.17), Arch (1.9.17) and Ubuntu 25.10 (sudo-rs
0.2.8) all agreed:

- a ticket made in one call is not visible from a new session, so it never
  reaches the next call;
- a ticket made by a helper process is not visible to that helper's caller;
- a ticket made by a process is visible to that same process's next sudo
  child.

So `clud sudo` is the parent of every sudo it runs.

**Flow** (`secret_prompt/sudo.rs`):

1. `sudo -n -v` checks first. If it succeeds (NOPASSWD), clud runs
   `sudo -n <command>` and never prompts. If sudo refuses the account
   ("may not run sudo", "not in the sudoers"), clud reports that and does not
   prompt.
2. clud starts one `sudo -S -p clud-sudo-password-request -v` and discards
   its output. sudo prints that marker each time it reads a password, on
   every flavour.
3. Each marker opens lokr's masked prompt, through the same path as askpass.
   lokr uses the desktop dialog, or its masked TTY prompt where a controlling
   terminal exists. clud writes the answer as one line to sudo's stdin pipe
   and keeps the pipe open, so sudo runs its own retry loop. clud opens at
   most three prompts. Closing stdin after one line would make sudo also try
   an empty password, and `pam_faillock` counts that try. On Arch, which
   ships `deny=3`, two typos then locked the account.
4. clud runs `sudo -n <command>`, inheriting stdin and relaying output line
   by line. If sudo keeps no ticket (`timestamp_timeout=0`), clud runs
   `sudo -S -p '' <command>` with the password as the only stdin line instead.
   Any relayed line that contains the password is redacted.
5. clud runs `sudo -k` to drop the ticket.

The password never appears in argv, the environment, the relayed output or
sudo's log. `-A`/`SUDO_ASKPASS` is not used because sudo-rs rejects `-A`; `-S`
works on classic sudo, sudo-rs and macOS sudo. Cancel exits 3 and runs nothing.
`clud sudo` refuses `-A`, `-S`, `-p`, `-k`, `-K`, `-v`, `-l`, `-e` and an
interactive root shell with no command, because it owns those options. Other
sudo options, such as `-u postgres`, pass through before the command. Windows
is not supported.

**Hook table.** The rule refuses plain `sudo cmd`, `sudo -v`, `sudo -l`,
`sudo -k cmd`, `sudo -S ...` and `sudo -A ...`, including inside `bash -c`,
`$(...)` and `env`. It allows:

- `sudo -n ...`;
- `sudo -k` or `sudo -K` with no command;
- `sudo -V` and `sudo --help`;
- `clud sudo ...`.

Override one call with `CLUD_BAD_CMD_OVERRIDE="sudo-prompt:<reason>"`. The rule
never runs on a Windows host, where `sudo` elevates through UAC.
clud.bot's remote sudo ([DD-216](../DESIGN_DECISIONS.md#dd-216-remote-sudo-is-validated-by-a-clud-session-leader-inside-the-operator-pty))
is unaffected: the session leader runs sudo itself, and an operator's shell is
not an agent hook context. Rationale:
[DD-217](../DESIGN_DECISIONS.md#dd-217-an-agents-sudo-password-comes-only-from-clud-sudo).

## Limits

- This prevents accidental exposure; it is not a sandbox. An agent with shell
  access under the same account can still read these locations.
- The flag is silently ignored on the Codex backend.
- `request_secret`'s prompt window has not been exercised end to end.
- lokr's prompt always adds "This is NOT your computer or login password." That
  line is wrong for a sudo request, which asks for the account password. Fixing
  it needs a lokr request option.
- clud's pinned, privately installed lokr supplies the MCP schema, scanner and
  prompt. Distribution and digest verification are owned by
  [secret-prompt.md](secret-prompt.md); no crates.io placeholder is used.

The scanner and MCP server live in the [lokr](https://github.com/forge-ci/lokr)
project; its `DESIGN.md` owns their internals. clud's integration is
`crates/clud-bin/src/secrets_catalog.rs`.

[forge-ci/lokr](https://github.com/forge-ci/lokr) is the secrets catalog.
Requests must name the secret and service/host so the catalog can find it later.
The clud integration delegates to the installed `lokr mcp`, `lokr daemon`, and
`lokr request` commands; lokr owns the prompt, catalog and MCP schema.

---
name: clud-secret
description: The blessed way to get any password, including sudo/root/administrator passwords, tokens and SSH private keys, through lokr's masked prompt. Use whenever a command needs sudo or root (for example after "a terminal is required", "a password is required" or "interactive authentication is required") or a task needs credentials, and run `clud sudo -- <command>` and never request or display a password in chat.
triggers:
  - When a command needs sudo, root or administrator privileges, or fails with "a terminal is required" or "a password is required"
  - When an authorized operation requires acquiring a password, token or SSH private key
---
<!-- managed-by: clud -->

## sudo: always `clud sudo`

When a command needs root, run it through `clud sudo` in a single call:

```bash
clud sudo -- apt-get install -y foo
clud sudo --reason 'Install the foo build dependency' -- apt-get install -y foo
clud sudo -- -u postgres psql -c 'select 1'
```

`clud sudo` first runs `sudo -n -v`. If sudo needs no password, the command
runs straight away with no prompt. Otherwise clud opens lokr's masked desktop
prompt (lokr's masked TTY prompt where a controlling terminal exists). It passes the
password only to sudo's stdin (`sudo -S -v`), then runs
`sudo -n <command>` and streams the command's output. The password is never in
argv, the environment, output or the transcript. It allows three password
attempts. Exit 3 means the user cancelled and nothing ran.

- Use one `clud sudo` per privileged command. A sudo ticket does not carry
  from one shell tool call to the next, so `sudo -v` followed by `sudo cmd`
  in a later call still fails.
- Plain `sudo cmd`, `sudo -S`, `sudo -A`/`SUDO_ASKPASS` and `sudo -v` are
  refused by clud's hook (`sudo-prompt`), and the refusal names the exact
  `clud sudo` command. `sudo -n ...`, `sudo -k` and `sudo -K` are allowed.
- Never ask the user to paste a password in chat, and never tell them to run
  the command themselves. If no prompt can open (no desktop and no terminal),
  report that limitation.
- Check the plan first with `clud sudo --dry-run -- <command>`.

## Other credentials: `clud secret`

If the `mcp__lokr__*` tools are present (`clud --allow-secrets`), call
`mcp__lokr__secret_status` first: the credential may already exist. It reports
names and locations, never values. Ask the user before reusing a key held by
another program, and use `mcp__lokr__request_secret` when the key is absent.
See <https://github.com/talos-hq/clud/blob/main/docs/architecture/secret-catalog.md>.

Use `clud secret` to acquire credentials without seeing them. Identify the
specific credential and service/host in required `--title`, explain why it is
needed and where to find it in required `--message` (alias of `--label`), and
choose the sink appropriate to the already-authorized operation:

```bash
clud secret --title 'GitLab token for gitlab.example.com' --message 'Authenticate glab; create a token in GitLab user settings > Access Tokens' --to-command glab auth login --hostname gitlab.example.com --stdin
clud secret --title 'SSH private key for the deployment host' --message 'Load the deployment identity into ssh-agent; copy the key from your secret manager' --multiline --to-command ssh-add -
clud secret --title 'OpenRouter API key for clud' --message 'Enable model requests; create a key at openrouter.ai/settings/keys' --vault openrouter
clud secret --title 'Telegram BotFather token for the notification bot' --message 'Enable bot notifications; copy the token from BotFather in Telegram' --hint 'digits:35 token characters' --pattern '[0-9]+:[A-Za-z0-9_-]{35}' --to-file /chosen/private/telegram-token
clud secret --title 'SSH private key for the deployment host' --message 'Save the deployment identity; copy the private key from your secret manager' --multiline --to-file /chosen/private/key
```

Every `clud secret` and `clud sudo` flag, with defaults and conflicts, is in the
[CLI grammar](https://github.com/talos-hq/clud/blob/main/docs/cli-grammar.md#clud-secret).
Preview the request with `--dry-run` before acquiring a credential. For tests,
use a fake lokr or fixtures, set `LOKR_TEST_MODE=1` and
`CLUD_TEST_NO_SECRET_UI=1`, and remove `DISPLAY`,
`WAYLAND_DISPLAY` and `WAYLAND_SOCKET`; never open real dialogs.

Use `--pattern <regex>` when the expected shape is known. It must match the
entire value; lokr allows at most three submissions in the same prompt
before failing without storing anything. Invalid regexes are rejected before prompting.
Place prompt options before `--to-command`, which consumes the remaining args.

The command reports `stored`, `failed`, or `cancelled` (exit 3). It suppresses
sink stdout and stderr because commands may echo their input. Verify success
with a command that reports authentication status without exposing the
credential. On cancellation, stop the authentication attempt. A single-line
request uses lokr, which can use a masked controlling TTY with context and warning.
If neither desktop nor TTY is available, report that limitation; do not ask for
the value in chat. One invocation opens at most one prompt.

`clud-askpass` implements GIT_ASKPASS and SSH_ASKPASS. Configure those
variables only for the intended git/ssh operation. For sudo use `clud sudo`:
sudo-rs rejects `-A`, so the SUDO_ASKPASS route is not portable. Do not invoke
the alias directly: its stdout is the authentication protocol for the calling
tool, and it refuses ordinary agent/tool callers.

Do not put credentials in command arguments, shell variables printed to the
transcript, or examples. If a real credential was already pasted in chat,
advise the user to rotate it. Prompting does not authorize a new login or an
unrelated external operation.

If implementing a credential-flow change, preserve focused RED -> GREEN
regression evidence for secrecy and the sink's behavior.

[forge-ci/lokr](https://github.com/forge-ci/lokr) is the secrets catalog.
Name the secret and its service/host in each request so lokr can find it later.
`clud secret` delegates to `lokr request`: lokr is both the prompt and catalog.
Native installers bundle functional lokr; release wheels download and verify
the same pinned helper on first use, then reuse its private cache.
`--dry-run` identifies its version and location without downloading or launching it. Do not
install crates.io lokr 0.0.1: it is not the functional prompt. Development builds
may use a functional release on PATH. Missing or modified managed lokr fails
with an install hint; clud never falls back to another prompt. File, `--to-keyring SERVICE USER`, and command sinks
belong to lokr; `--vault` uses lokr command stdin to clud’s internal vault writer.

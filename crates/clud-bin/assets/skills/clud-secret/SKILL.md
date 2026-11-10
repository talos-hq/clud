---
name: clud-secret
description: Acquire a password, token or SSH private key for an authorized operation using clud's masked native prompt and direct secret sinks. Use when a task needs credentials; never request or display their values in chat.
triggers:
  - When an authorized operation requires acquiring a password, token or SSH private key
---
<!-- managed-by: clud -->

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

Preview the request with `--dry-run` before acquiring a credential. For tests,
use fixtures, set `CLUD_TEST_NO_SECRET_UI=1`, and remove `DISPLAY`,
`WAYLAND_DISPLAY` and `WAYLAND_SOCKET`; never open real dialogs.

Use `--pattern <regex>` when the expected shape is known. It must match the
entire value; a mismatch fails without storing anything or opening another
prompt. Invalid regexes are rejected before prompting.
Place prompt options before `--to-command`, which consumes the remaining args.

The command reports `stored`, `failed`, or `cancelled` (exit 3). It suppresses
sink stdout and stderr because commands may echo their input. Verify success
with a command that reports authentication status without exposing the
credential. On cancellation, stop the authentication attempt. A single-line
request can use a masked interactive TTY with the same context and warning.
If neither desktop nor TTY is available, report that limitation; do not ask for
the value in chat. One invocation opens at most one prompt.

`clud-askpass` implements GIT_ASKPASS, SSH_ASKPASS and SUDO_ASKPASS. Configure
those variables only for the intended git/ssh/sudo operation. Do not invoke
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
The parallel `lokr request` CLI is not required by this flow; clud continues to
use its own prompt and sinks.

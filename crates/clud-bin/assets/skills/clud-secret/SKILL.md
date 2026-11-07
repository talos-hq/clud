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
host and purpose in `--label`, and choose the sink appropriate to the
already-authorized operation:

```bash
clud secret --label 'GitLab token for gitlab.example.com' --to-command glab auth login --hostname gitlab.example.com --stdin
clud secret --label 'SSH private key' --multiline --to-command ssh-add -
clud secret --label 'OpenRouter API key' --vault openrouter
clud secret --label 'Telegram BotFather token' --pattern '[0-9]+:[A-Za-z0-9_-]{35}' --to-file /chosen/private/telegram-token
clud secret --label 'SSH private key for this host' --multiline --to-file /chosen/private/key
```

Use `--pattern <regex>` when the expected shape is known. It must match the
entire value; a mismatch prompts again, up to three attempts, then fails
without storing anything. Invalid regexes are rejected before prompting.
Place prompt options before `--to-command`, which consumes the remaining args.

The command reports `stored`, `failed`, or `cancelled` (exit 3). It suppresses
sink stdout and stderr because commands may echo their input. Verify success
with a command that reports authentication status without exposing the
credential. On cancellation, stop the authentication attempt. If no desktop
prompt is available, report that limitation; do not ask for the value in chat.

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

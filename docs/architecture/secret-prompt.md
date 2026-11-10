# Secure credential prompt (#1708)

`clud secret` delegates to lokr’s masked credential prompt for an authorized
operation. It supports passwords/tokens and `--multiline` private-key input.
The prompt requires a specific credential and service/host in `--title`, plus
`--label` (also accepted as `--message`) explaining why it is needed and where
to find it. Empty or generic descriptions such as "password", "secret" or the
old defaults are rejected before acquisition. Optional `--hint` describes the
expected format. The window prominently displays the title, message, hint,
destination, and "This is NOT your computer or login password."

Choose exactly one sink:

- `--to-file PATH`: lokr’s private atomic writer creates owner-only
  files (0600 on Unix, protected owner DACL on Windows).
- `--to-keyring SERVICE USER`: lokr’s native OS keyring sink.
- `--vault deepseek|kimi|openrouter`: the provider's existing native vault
  record, with its stable service/account identifiers.
- `--to-command PROGRAM ARGS...`: send the exact value to command stdin,
  close the pipe, discard both output streams, and report the exit outcome.
  This option owns the rest of the command line, including flags.

For example:

```bash
clud secret --title 'GitLab token for gitlab.example.com' --message 'Authenticate glab; create a token in GitLab user settings > Access Tokens' --to-command glab auth login --hostname gitlab.example.com --stdin
clud secret --title 'SSH private key for the deployment host' --message 'Load the deployment identity into ssh-agent; copy the key from your secret manager' --multiline --to-command ssh-add -
```

Use `--dry-run` to preview the title, message, hint, warning and destination
without opening any prompt, reading any credential, or touching a sink.

Optional `--pattern <regex>` requires a full-value match before any sink is
called. lokr allows at most three submissions within the same prompt before failing without storage.
Invalid regexes produce a typed argument-validation error before prompting. Without
the flag, acquisition behavior is unchanged. Put it before `--to-command`.

The normal CLI and `clud-secret` alias print only `stored` (exit 0),
`cancelled` (exit 3), or `failed` (exit 1). Invalid options exit 2. Cancellation
leaves sinks untouched. A command sink's output is suppressed even on failure,
because authentication tools may echo input in their diagnostics. Its deadline
covers stdin writing, draining and waiting. Inputs are capped at 1 MiB, reject
NUL, and retain multiline bytes exactly. Empty input cannot be submitted.

`clud-askpass` implements the separate askpass protocol: configure it as
`GIT_ASKPASS`, `SSH_ASKPASS`, or `SUDO_ASKPASS` for the intended operation.
It returns the value through stdout to the requesting tool, as required by
that protocol. It refuses direct agent/tool callers and accepts git/ssh/sudo
ancestors through a bounded shell-wrapper chain. This is an accidental-exposure
guard, not a boundary against a malicious local process impersonating a caller.
Never run askpass directly from a captured agent shell.

The prompt and catalog belong to [forge-ci/lokr](https://github.com/forge-ci/lokr).
Published clud executables select a functional native lokr release, pinned by
version, exact size and SHA-256 in `crates/clud-bin/assets/lokr-release.json`.
The native installer extracts it offline under
`~/.clud/libexec/lokr/<version>-<digest>/` before committing clud. Native binaries
carry the bytes for offline extraction. Release wheels carry the exact URL,
size and digest, downloading and verifying the helper on first actual use;
later uses verify the cache without downloading. This keeps wheels below the
existing size limit. Resolution prefers that managed copy over sibling/PATH programs;
the non-functional crates.io lokr 0.0.1 is never the production dependency.
Static Linux clud includes GNU lokr for desktop dynamic loading and Nix-ld, plus
a musl fallback for hosts without the GNU loader. Windows/macOS carry their
matching native executable. `--dry-run` reports the selected engine/path but
never downloads or extracts it, starts it or writes the secret sink.

Development builds remain network-free and may use a functional sibling/PATH
lokr; their native installer refuses to claim success without pinned distribution data.
Missing or modified managed bytes fail closed with an install hint. clud never
opens zenity, systemd-ask-password or a separate credential UI.

`--title`, `--hint`, `--pattern`, `--multiline`, `--to-file`, `--to-keyring` and `--to-command`
map directly to `lokr request`; `--label`/`--message` map to `--message`.
`--vault` maps to `--to-command <clud> secret --internal-vault-stdin ... --vault PROVIDER`.
The hidden writer reads bounded, zeroized stdin and stores under clud's existing
provider service/account identifiers. Secret bytes never enter argv or logs.
lokr owns private file delivery, command stdin delivery and request cataloging.
clud discards lokr output and maps exit 0/3/other to stored/cancelled/failed.

Caller-checked askpass also acquires through lokr, using a temporary private
file removed after reading for its required stdout protocol.

The automatically discoverable `clud-secret` bundled skill documents sink
selection and status-only verification. Prompting does not itself authorize
logging into an unrelated service. Credentials already pasted into chat should
be rotated.

Tests never open credential windows. Run with
`env -u DISPLAY -u WAYLAND_DISPLAY -u WAYLAND_SOCKET LOKR_TEST_MODE=1`.
Debug integration tests use `CLUD_TEST_LOKR_BINARY` for a fake executable, or
legacy file/cancellation fixtures; missing fixtures fail closed.
`CLUD_TEST_NO_SECRET_UI=1` and pytest disable production resolution. The fake
records argv and stdin, returns controlled exits and emits sentinel output to
verify that clud only reports status. Use `--dry-run` for manual previews.

The named GitLab consumer acceptance uses the real `glab auth login --stdin`
and `glab auth status` commands with a synthetic token and a loopback GitLab API.
The fake acquisition fixture supplies the token directly to the login process.
The status command must make another authenticated request using the saved credential.
Both command outputs are captured and checked for the sentinel; the prompt's
agent-facing output remains `stored`. The fixture disables update checks,
clears inherited GitLab authentication/host overrides, and uses an isolated
configuration directory. Its explicit file-storage option applies only to the
synthetic test account and never changes the user's keychain.

`ci/install_glab.py` provisions GitLab CLI 1.124.0 for the six native unit-test
targets through the existing test workflow. Each official release archive has
a pinned SHA256; only its bounded regular executable is copied to the runner's
temporary directory. `CLUD_TEST_GLAB_BINARY` points the test to that executable.
Mandatory native coverage fails if it is missing. This dependency belongs to
CI; clud still ships one multicall executable and requires no GitLab CLI for
its other secret sinks.

[forge-ci/lokr](https://github.com/forge-ci/lokr) is the secrets catalog.
Name the secret and its service/host in each request so lokr can find it later.
`lokr request` owns the prompt and catalog; clud preserves its vault identifiers.

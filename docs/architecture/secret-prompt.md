# Secure credential prompt (#1708)

`clud secret` opens a native masked credential window for an authorized
operation. It supports passwords/tokens and `--multiline` private-key input.
The prompt requires a specific credential and service/host in `--title`, plus
`--label` (also accepted as `--message`) explaining why it is needed and where
to find it. Empty or generic descriptions such as "password", "secret" or the
old defaults are rejected before acquisition. Optional `--hint` describes the
expected format. The window prominently displays the title, message, hint,
destination, and "This is NOT your computer or login password."

Choose exactly one sink:

- `--to-file PATH`: the existing private atomic writer creates owner-only
  files (0600 on Unix, protected owner DACL on Windows).
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
called. A mismatch fails without storing anything or opening another prompt.
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

The native window uses eframe/egui with password widgets for single and
multiline input, built into the existing multicall executable. UI persistence
is disabled. Secret buffers are zeroized when released, and have no Debug or
serialization surface. No credential value is accepted on the command line.
When desktop prompts are unavailable, single-line requests can use a masked
TTY fallback only when stdin and stderr are terminals. It shows the same title,
message, hint, destination and login-password warning. Captured/piped sessions
and multiline input fail clearly without reading input or asking through chat.

On Linux, when the native graphics preflight is unavailable (#2197), a
single-line request without an interactive TTY selects the first available `zenity --entry --hide-text`,
`kdialog --password`, `ksshaskpass`, `pinentry` (Assuan `GETPIN`) that is on
PATH with a display present (`platform/unix/secret_helper.rs`). The value
returns on the helper's stdout to the same sink and is never logged. Multiline
input has no fallback. Only one backend is attempted per invocation; a failed native window or
helper never opens another prompt. Dismissing a helper is `cancelled`. Known GTK/Qt toolkit warnings on stderr do not
demote a silent exit 1 from cancel to unusable; a timeout ends the chain.

Known limitation: the helper's stdout is read through `running-process`
with `capture: true`, which retains the output in its history and queues and
frees them without wiping. clud clears those buffers right after reading and
wipes its own copies, but the crate's freed heap may still hold the secret
until it is reused. Tracked at
[zackees/running-process#1357](https://github.com/zackees/running-process/issues/1357)
(no-history or zeroizing capture); std::process is not an acceptable
workaround.

On Linux, the desktop runtime needs libxkbcommon; X11 additionally needs
libxkbcommon-x11. clud checks these before entering winit, whose X11 keyboard
loader otherwise panics on a missing library. A Wayland session does not
require the X11 library, including when XWayland also sets `DISPLAY`.
Linux CI installs the runtime and Xvfb so the native window probe executes.
Windows uses wgpu's Direct3D 12 backend, which can use the system WARP software
adapter. Its FXC shader compiler requires no additional compiler DLL. macOS
and Linux retain the OpenGL renderer.

The automatically discoverable `clud-secret` bundled skill documents sink
selection and status-only verification. Prompting does not itself authorize
logging into an unrelated service. Credentials already pasted into chat should
be rotated.

Tests never open credential windows. Debug builds with `CLUD_INTEGRATION_TESTS=1`
use only file-backed acquisition or cancellation fixtures; a missing fixture
fails closed. The obsolete `CLUD_TEST_SECRET_PROMPT_GUI` flag cannot opt out.
`CLUD_TEST_NO_SECRET_UI=1` also disables real GUI/helper/TTY acquisition in any
build, and pytest sets it for every child process while removing display
variables. The masked widgets render in an egui context without a window.
Run checks with `env -u DISPLAY -u WAYLAND_DISPLAY -u WAYLAND_SOCKET` and use
`--dry-run` for manual CLI previews. Single-run routing tests ensure a failed
native attempt cannot open a helper or TTY prompt, and format mismatches never
reopen acquisition. Real desktop/Wayland rendering is not claimed by these tests.

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
The parallel `lokr request` CLI is not required by this flow; clud continues to
use its own prompt and sinks.

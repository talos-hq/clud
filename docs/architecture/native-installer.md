# Native clud installer entry

Issue #1493 adds an early native entry point to the regular `clud` binary.
`--installer` opens a selector menu; `--install-current` and
`--install-version VERSION` choose an installation source, and `--yes` skips
interactive consent only when one of those choices is explicit. Existing
`--yes --clean-worktrees` behavior is unaffected. Installer flags are claimed
by both clap and the known/unknown argument splitter, so they cannot leak to
a backend agent.

`--installer --no-modify-path` requests binary-only installation. It composes
with the menu, current executable and exact published version choices; it
does not waive installation consent. The transaction still verifies the
binary's format, digest and exact version, but does not plan or apply profile
or User Path changes. Existing startup files need not be readable or writable
for this mode. The result explicitly reports **activation required** and does
not claim verified fresh name-based lookup. Exit 0 means the requested binary
installation completed; it does not mean the invoking terminal was activated.

For explicit current/version requests, `--installer-json` emits one
`clud-installer-result/v1` JSON object on stdout. Progress, confirmation UI and
terminal activation commands remain on stderr. The report includes the exit
code, selected version/destination, stable reason code and these states:

| Status | Meaning |
| --- | --- |
| `complete` | The binary was committed and fresh name-based lookup was verified. |
| `activation_required` | The binary-only request completed; activation was not verified. |
| `refused` | Required consent was unavailable, declined or canceled. |
| `failed` | Request validation, planning, installation or activation failed. |

`binary_committed` records an acknowledged transaction commit, while
`activation_verified` records successful fresh-shell verification. A refused
additional repair can report a committed binary with unverified activation.
Failure without an acknowledged commit does not assert that rollback had no
errors. Exit codes are preserved, so consumers must inspect `status` as well
as the exit code. For example:

```sh
clud --installer --install-current --yes --installer-json
```

The flag requires an explicit install choice; it does not change permission
requirements or admit the browser/menu output into the JSON stream.

After a binary commit, output includes labeled commands for Bash/zsh and fish
on POSIX, or PowerShell, Git Bash and Command Prompt on Windows. Labels avoid
guessing the invoking shell from the account login shell. The commands update
the chosen terminal's PATH without editing profiles or the registry. Command
Prompt opens a nested activated shell in the same terminal; exiting that shell
returns to the original environment. Its PowerShell bootstrap is encoded as
UTF-16LE to preserve literal `%` and `!` in directories even when the outer
cmd has delayed expansion enabled. The other commands use their shell's
literal quoting. Unrepresentable PATH entries receive an explicit diagnostic.
The Windows encoding and shell flags follow Microsoft's
[PowerShell CLI contract](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_powershell_exe?view=powershell-5.1)
and [cmd contract](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/cmd).

`main.rs::run` calls `self_install::entry` after argument normalization and
before utility dispatch, daemon startup, the runtime-cache hop, Windows
trampoline handling, and the console-title keeper. The early path keeps an
installer invocation independent of backend credentials and session setup.

A bare interactive invocation offers installation only when `clud` cannot be
resolved by name on the effective PATH. A found executable, even one different
from the invoking binary, suppresses that offer. The offer defaults to **Not
now** and declining continues ordinary startup. Explicit mode works regardless
of PATH. Noninteractive explicit mode requires a single choice and `--yes`;
otherwise it exits without writing.

All menus are selector models in `self_install/picker.rs` rendered by the
shared `selector.rs` terminal driver. The driver owns pending-input draining,
raw mode, CRLF output, viewport scrolling, Windows key-release filtering,
cursor restoration, and Escape/Ctrl-C handling. The release picker uses
`Catalog::compatible_releases`; its default follows the `latest-stable`
channel rather than assuming the first row is stable. Browser choice opens
`https://talos-hq.github.io/clud/install/index.html` and prints that URL if
the opener fails.

Public prerelease acceptance sets `CLUD_INSTALLER_CANDIDATE_TAG` to a numeric
tag and calls `--install-version` explicitly. The binary constructs the fixed
GitHub release URL for `installer-candidate-manifest.json`, requires the
requested candidate version to be complete and newer than the previous
`latest-stable`, then uses the same transaction and activation proof. An
ordinary catalog read rejects a candidate channel, so a prerelease cannot
enter normal discovery through a mixed catalog.

An accepted selection becomes either `InstallIntent::CurrentExecutable` or
`InstallIntent::Published(asset)`. `transaction.rs::plan` runs without writes,
records the source digest and destination state, and shows the exact plan
before consent. Only then does `execute` create the approved per-user bin
directory, acquire its install lock, stage a private executable, verify its
format, digest, and version, and commit it with a recoverable prior copy.
The running-executable path copies offline. The release path uses the exact
catalog asset, bounds and hashes its transfer, validates every redirect, and
extracts only one executable from a historical wheel. A matching direct
release can reuse the invoking executable only when its digest matches.

The read-only activation plan also shows the exact startup-file or Windows
User Path proposal before consent. On POSIX, activation uses owned stanzas in
the active Bash login and interactive files, zsh login and interactive files,
or a fish `conf.d` snippet with non-universal `fish_add_path`. Windows updates
only HKCU `Environment\\Path`, preserving its registry type and raw variable
references, then broadcasts the environment change. An ordinary install succeeds only
after a fresh shell or OS-built Windows user environment resolves `clud` by
name to the approved destination and returns the selected version. Shadows or
startup files that undo the edit produce a repair error, not success.
After a verified activation the installer prints how to pick up the new PATH
in the already-open terminal: `source` the first managed startup file (zsh's
`~/.zprofile`, which macOS Terminal's login shells read after `path_helper`),
or open a new terminal (#2743). On POSIX the approved destination is
`$CLUD_INSTALL_DIR/clud` when that variable is set (it must be absolute),
otherwise the first user-owned `~/.local/bin` or `~/bin` already on PATH, else
`~/.local/bin/clud`. The startup-file choice is a pure function of `HOME`,
`SHELL`, `ZDOTDIR` and `XDG_CONFIG_HOME`, unit-tested for macOS layouts in
`platform/unix/self_install_activation.rs`. `installer/install.sh` delegates
here rather than editing startup files itself
([installer-pages.md](installer-pages.md#installsh)).

If activation fails after an interactive install, the installer recomputes a
read-only activation plan and displays it for separate approval. **Not now**
is the default. Approval permits one additional activation attempt; refusal,
an unsafe plan, or a second failure leaves the binary installed but returns
failure. Cancellation returns 130. Automated `--yes` and noninteractive
invocations do not offer this extra prompt or authorize additional writes.
Repair retains prior-content checks and rollback and never removes another
package manager's executable.

The native candidate-host acceptance lane provisions Bash, fish and zsh on
Linux/macOS and sets `CLUD_TEST_REQUIRE_INSTALLER_SHELLS=1`. A missing supported
shell fails that lane instead of skipping it. Ordinary developer acceptance
may skip unavailable shells. Fish with a custom `XDG_CONFIG_HOME` and zsh
with a custom `ZDOTDIR` are checked in fresh login, interactive and combined
login/interactive shells for both the exact installed path and version.
Windows candidate and public checks independently launch PowerShell 5.1,
PowerShell 7, cmd and Git Bash using the persisted machine/user registry PATH.
Every shell must resolve the installed path by name and report the selected
version; the evidence records the resolved path for each shell. These hosted
checks do not establish clean consumer-machine or download-prompt behavior.

Candidate and public-release acceptance also require eight distribution/host
pairs: Arch and Alpine x64, plus Ubuntu, Debian and Fedora on both x64 and
ARM64. Each container runs on a matching native host and uses a pinned image
digest. Public aggregation rejects missing or duplicate pairs and compares
each record against the published digest for that architecture.
NixOS boots matching x64 and ARM64 VMs both with and without `nix-ld`.
Each case verifies its loader state before installation and still requires
the exact static musl executable. Public aggregation requires all four
architecture/loader-state combinations, rejecting missing or duplicate cases.

Operators can rerun the read-only public checks using the existing
`installer-check.yml` workflow's manual dispatch, with an exact `release_tag`
and `mode` (`candidate` or `released`). This validates downloaded assets and
the catalog; it does not publish, promote, delete or modify a release.
Released-mode host acceptance executes both an exact-version installation
and the interactive release picker's latest default, checking the selected
installed executable's digest and reported version against the independently
fetched public catalog. The picker transcript is retained beside its evidence.
Candidate mode retains exact-version selection because the stable channel
must continue to point at the previous public release.
Released-mode hosts also download the fixed older native installer 2.8.30,
verify its public catalog size/digest and reported source version, then drive
its release picker to install the independently expected newer stable version.
This checks discovery through the older installer's own catalog fetch and
picker. Evidence includes both source and installed digests
and a separate older-installer terminal transcript. The selected stable must
be newer than the fixed source version.

### First-run and browser acceptance boundaries

The native candidate-host suite drives the automatic first-run menu in a real
PTY/ConPTY with no `clud` on PATH. It verifies that **Not now** is selected and
that Escape and Ctrl-C exit before installation or startup writes. These
interrupt checks do not establish Enter-to-decline continuation.

The candidate-only `installer-ci-fixture` build can record the browser opener's
URL instead of opening a desktop browser. The suite selects **Open download
page**, requires the canonical install-page URL, and checks both successful
opening and a failed opener with a readable URL fallback. Release binaries
ignore this fixture variable. These checks establish menu routing and error
reporting; clean-machine browser download, quarantine and security prompts
still require consumer-machine evidence.

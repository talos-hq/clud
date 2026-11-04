# Native self-install catalog

`entry.rs` owns the early CLI and first-run gates. Explicit `--installer`
dispatch runs before the runtime-cache hop, Windows trampoline, daemon,
backend, title keeper, and launch setup. The automatic offer is limited to a
bare, interactive `clud` invocation when no executable named `clud` resolves
on the effective PATH. `picker.rs` supplies the menu, confirmation, and full
release-list models to the shared `selector.rs` terminal driver. The automatic
menu defaults to **Not now**; the explicit menu defaults to **Install this
version**. `transaction.rs` builds the read-only plan, verifies the selected
source, and commits one executable. `activation.rs` builds the read-only
shell plan shown before consent, updates owned Bash, zsh, or fish startup
files, and proves fresh login and interactive name-based lookup.
The host halves live behind the platform boundary (#1953):
`platform/unix/self_install_activation.rs` owns the Bash, zsh, and fish
startup-file edits and fresh-shell proof, and
`platform/windows/self_install_activation.rs` owns the HKCU User Path edit,
change broadcast, and OS-built user-environment proof on Windows.
`platform/{unix,windows}/self_install.rs` hold the owner, mode, and
reparse-point checks `transaction.rs` applies. A binary commit alone does not
count as installer success. Explicit `--no-modify-path` instead skips the
activation plan entirely, preserves profiles/User Path, and reports binary
installation with activation still required. After a verified interactive install, `setup.rs`
offers OpenRouter, Claude or Codex, stores an OpenRouter key and model,
saves the provider and native harness, and offers to start clud. `clud setup`
runs the same provider step ([setup-command.md](../../../../docs/architecture/setup-command.md)).
Scripted (`--yes`) installs and `--installer-json` reports skip it.

`result.rs` defines the versioned `clud-installer-result/v1` outcome. Add
`--installer-json` to an explicit current/version request to emit exactly one
JSON object on stdout; interactive diagnostics and activation commands remain
on stderr. Status distinguishes `complete`, `activation_required`, `refused`
and `failed`; `binary_committed` records transaction acknowledgement and
`activation_verified` records fresh-shell proof. A refused additional repair
can therefore report a committed binary without claiming activation. Exit
codes keep their ordinary meaning, including refusal/cancellation; code zero
alone is insufficient to distinguish complete installation from install-only
or a user declining the original confirmation. Invalid requests also receive
a structured failure when the installer reporting flag parsed successfully.

`catalog.rs` parses the bounded, duplicate-key rejecting v1 Catalog published
at the Pages installer endpoint. `Catalog::compatible_releases` lists complete
versions for a host, and `Catalog::resolve` chooses an exact version or the
`latest-stable` channel. Linux always prefers the static musl variant. A GNU
asset is eligible only after the caller supplies a verified non-NixOS GNU
loader and glibc 2.17+ result through `Host::gnu`.
`Catalog::parse_candidate` admits one complete newer candidate row only when
an explicit fixed-origin release tag selects the versioned public candidate
catalog; ordinary parsing rejects its candidate channel.

`ResolvedAsset::verify_bytes` checks the catalog size and SHA-256 before
installation. The selected musl asset must also be a matching ELF64 binary
without an interpreter or needed shared library. A failed selected asset is
an error; callers must not try another catalog variant after verification.

The host probe in `entry.rs` verifies the non-NixOS GNU loader and glibc floor
before it grants GNU eligibility. The published catalog generator remains in
`installer/catalog.py`; this module is the native reader of its output.
Static musl builds use the canonical GNU loader and `/usr/bin/getconf
GNU_LIBC_VERSION` to probe a GNU host without linking glibc into the
candidate. The `installer-ci-fixture` feature is compiled only into dev PR
candidate builds; it reads local catalog and asset bytes while retaining the
normal parser, digest, extraction, staging, and activation paths.

# Installer source

`catalog.py` verifies public release assets and builds the v1 install catalog.
`site.py` renders the GitHub Pages artifact and verifies same-origin download copies; `verify_site.py` checks its public
URL contract. `download.js` (inlined OS/arch detection) and `install.sh` (the
served macOS/Linux installer) are described in
[Download page detection](../docs/architecture/installer-pages.md#download-page-detection). See [Installer Pages architecture](../docs/architecture/installer-pages.md)
for the cross-file flow and the `bosn`/`act` validation command.
`release_assets.py` extracts Windows/macOS native executables from wheels;
`verify_native_assets.py` verifies those four files and the two separately
built static musl Linux binaries before publication. The release build flow
lives in [CI architecture](../docs/architecture/ci.md#release-profile-containment).
`candidate_catalog.py` attaches a separate versioned catalog to a public
prerelease while keeping the canonical stable pointer unchanged.
`homebrew_formula.py` renders the `talos-hq/homebrew-tap` formula for a promoted
release ([Homebrew tap](../docs/architecture/installer-pages.md#homebrew-tap)).

The repository root `install` file and the existing `install.sh` and
`install.ps1` are separate developer and shell installation routes; this
directory does not replace them.

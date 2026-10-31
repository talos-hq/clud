# Installer Pages and release catalog

Issue #1468 uses GitHub Pages as the installer discovery endpoint. The catalog
is a `manifest.json` v1 Catalog, not the repository's internal test-bundle
manifest. Its `online_url` identity field stays
`https://talos-hq.github.io/clud/install/manifest.json` (installed binaries
compare it verbatim), but nothing fetches that URL: the org's Pages custom
domain makes every `*.github.io` URL 301-redirect.

**Fetch order (#2449).** Clients and publishers read published metadata through
one ordered list per document — `crate::published_urls` in Rust,
`installer.catalog.INSTALL_CATALOG_URLS` and `models.publish.MODEL_MANIFEST_URLS`
in Python: the canonical domain (`https://talosbot.ai/clud/...`) first, then a
GitHub-owned fallback that survives losing the domain or its certificate. The
install catalog falls back to
`github.com/talos-hq/clud/releases/latest/download/installer-candidate-manifest.json`
(read in candidate mode, since `latest` only resolves to a published release);
the model manifest falls back to the `models-manifest.json` asset that
`model-pages.yml` uploads to the latest release, then the built-in table.
`ci/fetch_fallbacks.py` (`bash lint`) fails when a fetch site bypasses these
lists or a list stops ending with a GitHub-owned URL.

`installer/catalog.py` fetches the public GitHub release list and verifies each
candidate wheel or direct clud asset against GitHub's published SHA-256 and byte
size. Older wheel-only releases are advertised only when their archive contains
exactly one `clud`/`clud.exe` member with executable-format bytes. A direct
standalone binary takes precedence over a wheel for the same platform variant.
Historical Linux GNU assets carry `platform.libc=glibc` and
`variant.flavor=gnu`; verified static musl direct assets carry
`variant.flavor=static-musl` without a host libc requirement. The generator
rejects musl assets with an ELF interpreter or a needed shared library. The
canonical catalog excludes drafts and prereleases, retains every installable
stable version, and puts semantic versions newest first; `latest-stable` is a
channel pointer, not array position. A public prerelease has a separate
versioned `installer-candidate-manifest.json` attached to its final tag. That
catalog carries `channels.candidate` for the new version while
`channels.latest-stable` remains the previous public stable version.

The Rust reader in `crates/clud-bin/src/self_install/catalog.rs` accepts only a
bounded v1 clud Catalog with unique JSON keys, exact known platform variants,
matching release filenames and GitHub asset URLs, positive sizes, and SHA-256
digests. It rejects an incomplete or prerelease `latest-stable` pointer and
does not advertise incomplete historical releases. On Linux it selects a
static musl asset before GNU regardless of catalog row order. GNU-only history
requires an explicit host probe result proving a non-NixOS GNU loader and
glibc 2.17 or newer. The selected asset's size and digest are checked before
installation; a bad musl download is terminal rather than a reason to fall
back to GNU.

Candidate mode is opt-in through `CLUD_INSTALLER_CANDIDATE_TAG`. The native
binary validates a numeric release tag, fetches only the fixed GitHub release
asset URL, and parses a complete newer candidate row. Normal installs keep
using the canonical Pages URL and reject a candidate channel.

`installer/site.py` renders the exact Pages artifact paths: `index.html`,
`install/index.html`, and `install/manifest.json`. The project root is a static
client-side redirect to `/clud/install/index.html` with one hyperlink to the
catalog. The landing page lists only verified direct native assets from the catalog's
`latest-stable` release. It prefers static musl over GNU Linux assets and shows
an explicit no-download message for wheel-only releases. Each link uses the
catalog's immutable release URL, filename, OS, architecture, and variant; the
page does not construct `/releases/latest/download/` links. It provides
terminal launch commands for the downloaded executable and links to Apple's
Open Anyway guidance for macOS. The verifier checks those links against the
generated catalog.

`.github/workflows/install-pages.yml` runs the unit suite and upstream
`manifest-validate` on PRs, then publishes only on `main` or manual dispatch.
It fetches all three public URLs anonymously after deployment and verifies the
published page against the published catalog. The build job runs locally with
`bosn ci run --workspace . --workflow .github/workflows/install-pages.yml --job build-site --trigger pr --wait`
(bosn stubs `actions/configure-pages`); only the deployment API needs GitHub.
The full-history verification downloads about 5.5 GB across the 27 releases
present when this flow was introduced, so its build job has a 30-minute limit.

`auto-release.yml` snapshots the prior public release and Pages digest, uploads
final native bytes as a public prerelease, attaches the candidate catalog,
and calls `installer-check.yml` in candidate mode. Every native host and the
Arch, Alpine, Ubuntu, Debian, Fedora, and NixOS guests fetch public HTTPS bytes and prove a
fresh name-based install. Only after that gate passes does the workflow
publish PyPI, promote the same GitHub asset IDs, and deploy the canonical
Pages site. It calls the same installer matrix in released mode against the
new `latest-stable` pointer. A failed post-promotion gate demotes the release,
restores the prior latest release, redeploys the prior catalog pointer, and
leaves the workflow red if any restoration step fails. Publication shares a
serialized concurrency group with both Pages publishers.

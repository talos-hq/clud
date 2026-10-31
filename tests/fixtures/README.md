# Pinned Codex installer fixture (#1461)

`codex-install-150e3cf6.b64` is the byte-for-byte standalone installer served
at `https://chatgpt.com/codex/install.sh` on 2026-09-26, stored as base64 so
the repository's command-scan hook does not mistake the installer's deletion
commands for commands being executed by the agent. Its decoded SHA-256 is
`150e3cf675682efeaac115aa3747add3f27887896d04ce6d0b56478d8b428bf6`.

`tests/test_codex_installer_rm.py` verifies that hash before use. It patches
only `RELEASES_BASE_URL` to a `file://` directory built under `tmp_path`. That
directory contains release metadata, a SHA-256 manifest, and a minimal local
package tarball with executable stand-ins; the installer's download, checksum,
staging, symlink, cleanup, and replacement paths remain upstream code. A
`curl` guard rejects HTTP(S), so a fixture mistake cannot fall back to GitHub
or another network source. The installer is piped to a non-interactive `sh`
from a fake Codex backend launched through clud's foreground and daemon child
environments. All resulting paths are under a temporary home inside bosn.

`gh_watch_harness.py` is the offline child invoked through the session `gh`
alias by `tests/test_gh_shim.py`. It runs the actual bundled PR watcher with
network edges replaced by deterministic failure, review, and no-checks
snapshots, recording scoped cancellation under the test's temporary directory.

For #2004, the same offline watcher fixture also covers green, timeout and
required-versus-advisory scope, with an optional deterministic stdin handshake
for controlled process sessions. Its watchdog scenario invokes the real clud
tool wrapper with a blocked local uv fixture. GitHub reads not replaced by the
fixture fail immediately. This tests the documented execution procedure, not
a real Codex agent or automatic goal resumption.

`grind_workflow_probe.mjs` executes the production `grind-run.js` body in a
controlled async JavaScript host (#1897). It removes only the metadata export
keyword and supplies deterministic agent replies. Prompts and feature-landing
commands are captured without model, shell-tool or GitHub requests. It tests
workflow data flow rather than the real Claude Workflow host. The companion
Python tests also exercise real hook caps, task separators and offline waiter
resumption, with a fork/upstream command-target fixture. An optional `script` maps label prefixes to ordered replies (with
`delay_ms`), which `tests/test_grind_queue_landing.py` uses to drive the
merge-queue enqueue, chaperone and fixup paths (#2467).

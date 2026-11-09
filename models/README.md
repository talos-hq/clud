# Model Pages publisher

`manifest.py` defines the small model document and strict stable-family
selection. `publish.py` merges public OpenRouter and models.dev observations
with the last published document, then stages a complete
Pages artifact without rebuilding the installer history, and `preserve.py`
keeps the current model document when the installer site is rebuilt. The
contract and runtime consumer are in [model-pages.md](../docs/architecture/model-pages.md).

`selection-policy.json` lists model IDs the ChatGPT Codex backend has rejected.
The publisher removes these IDs from both fresh catalog observations and the
previously published manifest. This lets an operator roll back the server
default without requiring existing clud clients to upgrade, and prevents the
next nightly publication from undoing the rollback. Remove an ID only after a
live ChatGPT backend probe succeeds.

`crates/clud-bin/assets/codex-models.json` is the bundled stable-slug snapshot. Refresh it with
`python -m models.refresh_openrouter`; the command reads OpenRouter’s public
`/api/v1/models` endpoint and requires Sol, Terra, and Luna rows.

The Sol 6.1 block was removed after `codex-cli 0.164.0` successfully completed
an authenticated ChatGPT Codex request with `gpt-6.1-sol` at low effort on
2026-10-27 UTC. The same probe succeeded for `gpt-6.1-terra`.

The upstream client's advertised compatibility version and the existing Bosn
Codex CLI pin must match this verified probe version. A `codex_upstream` guard
test enforces that relationship: the ChatGPT backend uses the `version` header
to gate model access (#2299). Update the record and both consumers together
when verifying a newer Codex release.

Model-only publication also preserves same-origin binary links from the installer
page; see [download naming](../docs/architecture/installer-pages.md#download-page-detection).

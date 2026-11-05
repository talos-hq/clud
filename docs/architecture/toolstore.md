# Toolstore: versioned names for the one `clud` binary (#2558)

clud answers to many names ([DD-121](../DESIGN_DECISIONS.md#dd-121-helper-executables-are-argv0-aliases-of-the-one-clud-binary)).
The names clud owns under `~/.clud/state` — `helper-bin/{clud-cmd-scan,clud-block-bad-cmd}`,
`shims/*` and `rm-shim/*` — point into a content-addressed store instead of at
whichever file the launcher is. Code: `crates/clud-bin/src/toolstore.rs`.
Rationale: [DD-203](../DESIGN_DECISIONS.md#dd-203-clud-owned-names-point-into-a-content-addressed-toolstore).

```text
~/.clud/toolstore/<build-id>/clud        one immutable binary per build
~/.clud/toolstore/<build-id>/.pins/<pid> launches that published or used it
~/.clud/toolstore/.ids/<key>             memoized build id per (path, size, mtime)
~/.clud/state/helper-bin/clud-cmd-scan -> ~/.clud/toolstore/<build-id>/clud
```

The store is **additive**: no existing `~/.clud` path moved. Consolidating the
`~/.clud` layout is #1963 (meta) and out of scope here.

## Build id

`<CARGO_PKG_VERSION>+<12 hex>`, from the ELF `NT_GNU_BUILD_ID` note when the
binary has one, otherwise the SHA-256 of its bytes; sanitized to one portable
path component (`toolstore::build_id`). Two builds of the same size get
different ids, which the old size/mtime fingerprint could not do.

## Lifecycle

| Step | Function | Rule |
| --- | --- | --- |
| publish | `toolstore::publish` | copy into a temp dir, rename into place; an existing build dir is never written or replaced |
| point | `toolstore::install_owned` | staged symlink + rename (atomic); fallback is the `alias_link` hardlink/copy ladder with a `.<name>.clud-build` sidecar naming the build |
| freshness | `toolstore::resolved_build` / `is_current` | exact: the build a name resolves to equals the launcher's id |
| doctor | `doctor::wrong_builds` | one `wrong_build` finding per owned name on another build (or none); `--fix` re-points it |
| gc | `toolstore::prune` (daemon GC tick) | delete builds that are not the launcher's, not named, not pinned by a live pid, and not among the newest 2; renamed aside before deletion |

Callers: `block_bad_cmd_rollout::expose_helper_at` / `refresh_owned_helper`
(helper-bin only), `shim_install::extract_shims_at`, `install_rm_with` and
`refresh_running_session_aliases_at`, and `doctor::repair` for owned dirs.
Foreign installs — pip's `~/.venv/bin/clud`, `uv tool`'s `bin/clud`, and the
aliases beside the launcher in an installer-owned bin dir — are never pointed
into the store; `foreign_helper_warning` still reports a foreign first
`clud-cmd-scan` on PATH. `clud __link-aliases <dir>` keeps the plain
`alias_link::install_alias` ladder for build outputs.

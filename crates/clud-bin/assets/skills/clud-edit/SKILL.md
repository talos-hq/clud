---
name: clud-edit
description: Edit TOML values (including array-of-tables selectors), Nix attributes, Markdown table rows, or an exact text span through the bundled fail-loud edit/edit.py tool. Use it instead of inline Python `s.replace(...)` / `s.index(...)` splices or sed for config edits.
triggers:
  - When changing a value in a .toml, .nix or Markdown table file
  - When about to write a heredoc Python string splice or sed substitution to edit a file
---
<!-- managed-by: clud -->

# /clud-edit

Prefer these verbs over inline string splicing. A heredoc `s.replace(old, new)`
silently does nothing when the text drifted, and `s.index(...)` edits the
first occurrence even when it is the wrong one. Every verb here resolves
exactly one target or exits 2 with the reason (missing, ambiguous, or an
edit that would not round-trip). Exit 64 is a usage error. `--dry-run`
prints the unified diff and writes nothing. A real edit prints the diff too.

The launcher exports `CLUD_EXE`; the commands below are Bash syntax.

```bash
E() { "$CLUD_EXE" tool run edit/edit.py -- "$@"; }

# TOML: get/set/delete by path. `name[key=value]` or `name[N]` picks an
# array-of-tables element. VALUE is a TOML literal (8, true, "s", [1,2]);
# --string takes raw text, --create adds a missing key. Comments and every
# untouched line stay byte-for-byte.
E toml fleet.toml set 'host[name=dell-jw22zc3].lane[name=obs-rust].dispatch.max' 8
E toml fleet.toml get 'host[0].lane[name=obs-rust].dispatch.max'
E toml fleet.toml delete 'defaults.max' --dry-run

# Nix: full attribute path; resolves `a.b = ...;`, nested blocks and inline
# `{ a = 1; b = 2; }` sets. EXPR goes in verbatim. Refuses ambiguous paths.
E nix hosts/x/default.nix set 'services.gh-runner-host.lanes.obs-rust.dispatch.max' 8
E nix hosts/x/default.nix set 'networking.hostName' '"x"'

# Markdown table row keyed by a column (default 1; backticks ignored).
E md docs/policy.md table-row --after-key GATE-014 --row '| `GATE-018` | ... |'
#   also --before-key K, --replace-key K, --append; --table N, --key-column N

# Exact-text fallback: fails unless the match count equals --expect-count
# (default 1). Use --old-file/--new-file to skip shell quoting.
E text src/x.rs replace --old-file old.txt --new-file new.txt --expect-count 1
```

Use `text replace` only when no structured verb fits, and keep `--old`
long enough to be unique. For multi-file search/replace blocks, use
`code/patch.py`.

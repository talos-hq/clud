"""Which PR changes need the Windows x64 lanes without a `ci-windows` label.

Routine PR CI and the merge queue run Linux x64 only. Three Windows regressions
in hook path handling and PATH-stubbing Python tests merged that way and were
first caught by a release's exact-SHA full CI (#2831, #2844): #2737, #2788, #2683,
#2827. A label is chosen once, at PR creation, so agents rarely pick it. The CI
resolver (`ci/ci_matrix.py`) therefore asks this module whether the PR's
changed files touch a path-sensitive area; if so the PR runs the `windows`
mode, exactly the lanes `ci-windows` adds, and `CI OK` requires them.

Two checks, one owner:

* `WINDOWS_SENSITIVE_PATHS` -- areas whose code parses, joins or resolves host
  paths, executables or PATH. Patterns use `fnmatch` semantics: `*` also
  matches `/`, so `dir/*` covers a whole subtree.
* `python_test_hazards` -- a content check for changed Python tests that put
  an extensionless executable on PATH or assert POSIX execute bits. Windows
  can run neither (#2683, #2827). About forty existing tests use these
  patterns, many correctly (Windows-skipped, `.cmd` shims), so a blocking lint
  would need a guard-aware parser or a forty-file allowlist; running the real
  Windows suite on such a change is cheaper and is the ground truth.

Design: docs/architecture/ci.md#automatic-windows-lanes-for-path-sensitive-changes-dd-224
(DD-224).
"""

from __future__ import annotations

import re
from collections.abc import Callable, Iterable
from fnmatch import fnmatchcase

_SRC = "crates/clud-bin/src/"

#: The single list of path-sensitive areas. Keep each entry narrow: every match
#: costs a PR the Windows x64 build and both Windows suites.
WINDOWS_SENSITIVE_PATHS: tuple[str, ...] = (
    # Bash-command guards: they parse paths and file words out of commands
    # (#2737 `resolvable_path`, #2788 `file_key`).
    f"{_SRC}block_bad_cmd*",
    # Hook dispatch: resolves hook roots and script paths.
    f"{_SRC}clud_hooks*",
    f"{_SRC}clud_hook_roots*",
    f"{_SRC}hook_payload_guard.rs",
    # Deletion guards and the rm tool: path identity, trash and home files.
    f"{_SRC}rm_guard.rs",
    f"{_SRC}rm_tool*",
    # Home resolution and path utilities.
    f"{_SRC}home.rs",
    f"{_SRC}path_norm.rs",
    f"{_SRC}safe_mktemp*",
    f"{_SRC}fs_private.rs",
    # Shims, PATH and the multicall executable names.
    f"{_SRC}shim_*",
    f"{_SRC}multicall.rs",
    f"{_SRC}alias_link.rs",
    # Every crate's platform boundary (DD-175).
    "crates/*/src/platform/*",
)

#: Only Python tests get the content check; the tool under test is matched by
#: path above when it is path-sensitive.
_PYTHON_TEST = "tests/*.py"

_EXEC_BIT_ASSERT = re.compile(
    r"st_mode\s*&\s*(?:0o[0-7]*[1357][0-7]*\b|stat\.S_IX(?:USR|GRP|OTH)|stat\.S_IEXEC)"
    r"|os\.X_OK"
)
_MAKES_EXECUTABLE = re.compile(r"chmod\(.*(?:stat\.S_IEXEC|stat\.S_IX\w+|0o[0-7]*[1357][0-7]*\b)")
_EDITS_PATH = re.compile(
    r"""setenv\(\s*["']PATH["']|environ\[\s*["']PATH["']\s*\]\s*=|["']PATH["']\s*:"""
)


def sensitive_path(path: str) -> str | None:
    """The `WINDOWS_SENSITIVE_PATHS` pattern `path` matches, if any."""
    return next((p for p in WINDOWS_SENSITIVE_PATHS if fnmatchcase(path, p)), None)


def python_test_hazards(text: str) -> list[str]:
    """Windows portability hazards in one Python test's source."""
    hazards = []
    if _MAKES_EXECUTABLE.search(text) and _EDITS_PATH.search(text):
        hazards.append("puts an executable stub on PATH")
    if _EXEC_BIT_ASSERT.search(text):
        hazards.append("checks POSIX execute bits")
    return hazards


def windows_reasons(changed: Iterable[str], read: Callable[[str], str | None]) -> list[str]:
    """Why these changed files need the Windows lanes; empty when they do not.

    `read` returns a changed file's text at the PR head, or None when the PR
    deleted it (a deleted test cannot break on Windows).
    """
    reasons = []
    for path in changed:
        pattern = sensitive_path(path)
        if pattern:
            reasons.append(f"{path} matches {pattern}")
            continue
        if not fnmatchcase(path, _PYTHON_TEST):
            continue
        text = read(path)
        if text is not None:
            reasons.extend(f"{path} {hazard}" for hazard in python_test_hazards(text))
    return reasons

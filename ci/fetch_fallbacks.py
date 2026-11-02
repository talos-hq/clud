"""Every published-metadata fetch has a GitHub-owned fallback (#2449).

clud's install catalog and model manifest are published on the org's GitHub
Pages custom domain. When the org added that domain, every ``*.github.io``
URL started 301-redirecting, the certificate lagged, and every installed clud
and every PR's unit shards broke at once (#2433). The fix is one ordered URL
list per document: the canonical domain first, a GitHub-owned URL
(``github.com/.../releases/latest/download/...`` or ``raw.githubusercontent.com``)
last. This guard keeps it that way:

* A published-metadata URL (``https://<host>/clud/install/manifest.json`` or
  ``.../clud/models/manifest.json``) may appear only in a registry file or an
  allowlisted file below. Any other occurrence is a fetch site that bypasses
  the fallback list.
* Every registry list must end with a GitHub-owned URL and must not fetch a
  ``*.github.io`` URL (it redirects to the custom domain).

Tests (``tests/``, ``*_tests.rs`` and a trailing ``#[cfg(test)] mod tests``)
are not scanned. Run via ``bash lint`` (see ``ci/lint.py``).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

from ci.process import check_output

ROOT = Path(__file__).resolve().parents[1]

PUBLISHED_URL = re.compile(r"https://[A-Za-z0-9.-]+/clud/(?:install|models)/manifest\.json")
GITHUB_OWNED = ("https://github.com/", "https://raw.githubusercontent.com/")
TEST_MODULE = re.compile(r"^\s*mod tests\b")

#: Registry file -> the names of the ordered URL lists it must define.
REGISTRIES = {
    "crates/clud-bin/src/published_urls.rs": ("INSTALL_CATALOG", "MODEL_MANIFEST"),
    "installer/catalog.py": ("INSTALL_CATALOG_URLS",),
    "models/publish.py": ("MODEL_MANIFEST_URLS",),
}

#: Files that may name a published URL without fetching it, each for a reason.
ALLOWED_FILES = frozenset(
    {
        # The catalog's `online_url` identity field, checked against the
        # accepted values; already-installed binaries compare it verbatim.
        "crates/clud-bin/src/self_install/catalog.rs",
        # Publication smoke checks: they verify what Pages serves, so they
        # must read the domain itself, never a fallback.
        ".github/workflows/auto-release.yml",
        ".github/workflows/install-pages.yml",
        ".github/workflows/model-pages.yml",
        "ci/installer_public_distro_guest.sh",
        "ci/nixos/public-guest.sh",
        # This guard's own docstring.
        "ci/fetch_fallbacks.py",
    }
)

SCANNED_ROOTS = (".github/", "ci/", "crates/", "installer/", "models/", "testbins/")
SCANNED_SUFFIXES = (".rs", ".py", ".sh", ".ps1", ".yml", ".yaml", ".toml", ".js")
ROOT_SCRIPTS = frozenset({"install", "install.ps1", "install.sh", "build", "lint", "test"})

REASON = (
    "fetch published metadata through the ordered fallback lists "
    "(`crate::published_urls`, `installer.catalog.INSTALL_CATALOG_URLS`, "
    "`models.publish.MODEL_MANIFEST_URLS`), which end with a GitHub-owned URL (#2449)."
)


def tracked_files() -> list[str]:
    out = check_output(["git", "ls-files", "-z"], cwd=ROOT)
    return [p for p in str(out).split("\0") if p]


def is_scanned(rel: str) -> bool:
    if rel.startswith(("vendor/", "dylints/")) or "/tests/" in f"/{rel}":
        return False
    if rel.endswith("_tests.rs"):
        return False
    if rel in ROOT_SCRIPTS:
        return True
    return rel.startswith(SCANNED_ROOTS) and rel.endswith(SCANNED_SUFFIXES)


def production_text(text: str) -> str:
    """Drop a trailing Rust unit-test module (the file-closing convention)."""
    lines = text.splitlines()
    previous = ""
    for index, raw in enumerate(lines):
        if TEST_MODULE.match(raw) and previous.strip() == "#[cfg(test)]":
            return "\n".join(lines[: index - 1])
        if raw.strip():
            previous = raw
    return text


def stray_urls(rel: str, text: str) -> list[tuple[int, str]]:
    """(line number, line) for each published URL outside a registry."""
    if rel in REGISTRIES or rel in ALLOWED_FILES:
        return []
    return [
        (number, raw.strip())
        for number, raw in enumerate(production_text(text).splitlines(), start=1)
        if PUBLISHED_URL.search(raw)
    ]


def list_body(text: str, name: str) -> str | None:
    """The bracketed body of `name`'s list/tuple literal, or None."""
    match = re.search(rf"\b{name}\b[^=\n]*=\s*&?[\[(]", text)
    if not match:
        return None
    opener = text[match.end() - 1]
    closer = "]" if opener == "[" else ")"
    end = text.find(closer, match.end())
    return None if end < 0 else text[match.end() : end]


def registry_problems(rel: str, text: str) -> list[str]:
    problems = []
    for name in REGISTRIES[rel]:
        body = list_body(text, name)
        if body is None:
            problems.append(f"{name} is not defined as a URL list")
            continue
        # Python f-strings name the domain through a constant; resolve the
        # one prefix this repo uses so the order check still sees a URL.
        urls = re.findall(r"""f?"((?:https://|\{PAGES\})[^"]*)\"""", body)
        if len(urls) < 2:
            problems.append(f"{name} needs the domain and a GitHub fallback, has {urls}")
            continue
        if not urls[-1].startswith(GITHUB_OWNED):
            problems.append(f"{name} must end with a GitHub-owned URL, ends with {urls[-1]}")
        if any(".github.io/" in url for url in urls):
            problems.append(f"{name} fetches a *.github.io URL, which redirects")
    return problems


def main() -> int:
    total = 0
    for rel in tracked_files():
        if not is_scanned(rel):
            continue
        try:
            text = (ROOT / rel).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        for number, line in stray_urls(rel, text):
            print(f"{rel}:{number}: BANNED published URL outside a fallback list — {REASON}",
                  file=sys.stderr)
            print(f"  {line}", file=sys.stderr)
            total += 1
        if rel in REGISTRIES:
            for problem in registry_problems(rel, text):
                print(f"{rel}: {problem} — {REASON}", file=sys.stderr)
                total += 1
    missing = [rel for rel in REGISTRIES if not (ROOT / rel).is_file()]
    for rel in missing:
        print(f"{rel}: registry file is missing — {REASON}", file=sys.stderr)
        total += 1
    if total:
        print(f"\n{total} published-metadata fallback problem(s).", file=sys.stderr)
        return 1
    print("Every published-metadata fetch has a GitHub fallback.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

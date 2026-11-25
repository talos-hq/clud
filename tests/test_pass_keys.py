"""Pass-marker keys hash exactly the inputs each lint lane reads (#2078).

A main push that only edits CI helpers, other workflows or repo config must
reuse the Dylint and Clippy markers; an edit to the lane's own workflow, the
scripts it runs, or any Rust input must miss them.
"""

from __future__ import annotations

import re
import shutil
from pathlib import Path

import pytest

from ci.process import run

ROOT = Path(__file__).resolve().parent.parent
FILTER_RE = re.compile(r"digest=\$\(git ls-files -s \| (awk .*?) \| sha256sum")

SHARED_IGNORED = [
    "ci/test.py",
    "ci/run_bundle.py",
    ".github/workflows/ci.yml",
    ".github/workflows/_run-tests.yml",
    "ci.toml",
    "local-gate.toml",
    "bosn.toml",
    "docs/architecture/ci.md",
    "bench/README.md",
    "CLAUDE.md",
    "tests/test_unit_shards.py",
]
SHARED_KEPT = [
    "crates/clud-bin/src/main.rs",
    "dylints/enforce_platform_boundary/src/lib.rs",
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    "tests/fixtures/openrouter_catalog.json",
    "uv.lock",
]


def kept_paths(workflow: str, paths: list[str], tmp_path: Path) -> set[str]:
    text = (ROOT / ".github/workflows" / workflow).read_text(encoding="utf-8")
    match = FILTER_RE.search(text)
    assert match, f"no pass-key filter in {workflow}"
    listing = tmp_path / "ls-files.txt"
    rows = "".join(f"100644 {'0' * 40} 0\t{path}\n" for path in paths)
    listing.write_text(rows, encoding="utf-8")
    command = match.group(1) + ' < "$1"'
    result = run(
        ["bash", "-c", command, "pass-key", str(listing)],
        capture_output=True,
        text=True,
        check=True,
    )
    return {line.split("\t", 1)[1] for line in result.stdout.splitlines()}


NO_SHELL = shutil.which("awk") is None or shutil.which("bash") is None


@pytest.mark.skipif(NO_SHELL, reason="needs bash and awk")
@pytest.mark.parametrize(
    ("workflow", "own_inputs", "foreign"),
    [
        (
            "_dylint.yml",
            [".github/workflows/_dylint.yml"],
            [".github/workflows/_build-target.yml", "ci/xbuild.py"],
        ),
        (
            "_build-target.yml",
            [
                ".github/workflows/_build-target.yml",
                ".github/actions/setup-build/action.yml",
                "ci/__init__.py",
                "ci/xbuild.py",
                "ci/cargo_messages.py",
                "ci/process.py",
                "ci/sync_build_python.sh",
            ],
            [".github/workflows/_dylint.yml"],
        ),
    ],
)
def test_pass_key_hashes_only_the_lane_inputs(
    workflow: str, own_inputs: list[str], foreign: list[str], tmp_path: Path
) -> None:
    paths = SHARED_IGNORED + SHARED_KEPT + own_inputs + foreign
    assert kept_paths(workflow, paths, tmp_path) == set(SHARED_KEPT + own_inputs)


def test_clippy_key_names_the_cross_strategy() -> None:
    text = (ROOT / ".github/workflows/_build-target.yml").read_text(encoding="utf-8")
    assert "pass-v1-clippy-${{ inputs.target }}-${{ inputs.strategy }}-" in text

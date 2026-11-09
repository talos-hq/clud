"""`clud` is multicall: helper names are argv[0] aliases of the one binary (#1551)."""

from __future__ import annotations

import json
import os
import shutil
import sys
from pathlib import Path

import pytest

from tests import process

SUFFIX = ".exe" if sys.platform == "win32" else ""
DENY = json.dumps({"tool_name": "Bash", "tool_input": {"command": "bad" + " cmd"}})


def _clud() -> Path:
    value = os.environ.get("CLUD_TEST_BINARY")
    path = (
        Path(value)
        if value
        else Path(__file__).resolve().parents[1] / "target/debug" / f"clud{SUFFIX}"
    )
    assert path.is_file(), path
    return path


def _link(kind: str, source: Path, target: Path) -> None:
    if kind == "hardlink":
        try:
            os.link(source, target)
        except OSError:
            pytest.skip("hardlinks across filesystems are refused (EXDEV)")
    elif kind == "symlink":
        try:
            os.symlink(source, target)
        except OSError:
            pytest.skip("symlinks need Developer Mode on Windows")
    else:
        shutil.copy2(source, target)


@pytest.mark.parametrize("kind", ["hardlink", "symlink", "copy"])
@pytest.mark.parametrize("spelling", ["clud-shim", "CLUD-SHIM"])
def test_shim_alias_answers_to_its_name(tmp_path: Path, kind: str, spelling: str) -> None:
    alias = tmp_path / f"{spelling}{SUFFIX}"
    _link(kind, _clud(), alias)
    result = process.run([str(alias), "--registry"], capture_output=True, text=True, timeout=30)
    assert result.returncode == 0, result
    assert "abi" in json.loads(result.stdout)


@pytest.mark.parametrize("kind", ["hardlink", "symlink", "copy"])
@pytest.mark.parametrize("name", ["clud-cmd-scan", "clud-block-bad-cmd"])
def test_scanner_aliases_run_the_scanner(tmp_path: Path, kind: str, name: str) -> None:
    alias = tmp_path / f"{name}{SUFFIX}"
    _link(kind, _clud(), alias)
    result = process.run(
        [str(alias)],
        input=DENY,
        capture_output=True,
        text=True,
        timeout=30,
        env={**os.environ, "HOME": str(tmp_path), "USERPROFILE": str(tmp_path)},
    )
    assert result.returncode == 2, result
    assert "deny" in result.stdout


def test_hidden_subcommands_reach_the_same_entry_points(tmp_path: Path) -> None:
    clud = str(_clud())
    env = {**os.environ, "HOME": str(tmp_path), "USERPROFILE": str(tmp_path)}
    scan = process.run(
        [clud, "__cmd-scan"], input=DENY, capture_output=True, text=True, timeout=30, env=env
    )
    assert scan.returncode == 2, scan
    shim = process.run(
        [clud, "__shim", "clud-shim", "--registry"], capture_output=True, text=True, timeout=30
    )
    assert shim.returncode == 0, shim
    assert "abi" in json.loads(shim.stdout)


def test_link_aliases_materializes_every_alias(tmp_path: Path) -> None:
    result = process.run(
        [str(_clud()), "__link-aliases", str(tmp_path / "out")],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 0, result
    names = {path.name.removesuffix(".exe") for path in (tmp_path / "out").iterdir()}
    assert {"clud-cmd-scan", "clud-block-bad-cmd", "clud-shim", "rm", "gh", "safe-rm"} <= names
    # Running it again is a no-op that still succeeds.
    again = process.run(
        [str(_clud()), "__link-aliases", str(tmp_path / "out")],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert again.returncode == 0, again


@pytest.mark.parametrize(
    "spelling", ["clud-installer", "clud-install", "clud-setup", "CLUD-SETUP (1)"]
)
@pytest.mark.parametrize("flags", [[], ["--installer"]])
def test_installer_alias_enters_installer_before_backend(
    tmp_path: Path, spelling: str, flags: list[str]
) -> None:
    alias = tmp_path / f"{spelling}{SUFFIX}"
    _link("hardlink", _clud(), alias)
    result = process.run([str(alias), *flags], capture_output=True, text=True, timeout=30)
    assert result.returncode == 2
    assert "a terminal is required" in result.stderr

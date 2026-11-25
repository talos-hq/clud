"""Tests for the shared Dell wheelhouse bootstrap."""

from __future__ import annotations

import os
import sys
import tarfile
from pathlib import Path

import pytest

from ci import process

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "ci" / "sync_build_python.sh"
pytestmark = pytest.mark.skipif(sys.platform != "linux", reason="Dell runner shell contract")


@pytest.fixture
def runner(tmp_path: Path) -> tuple[dict[str, str], Path]:
    tools = tmp_path / "tools"
    tools.mkdir()
    (tmp_path / "assets").mkdir()
    (tmp_path / "uv.lock").write_text("a distinct lock\n", encoding="utf-8")
    log = tmp_path / "commands"
    uv = tools / "uv"
    uv.write_text(
        '#!/usr/bin/env bash\n'
        'printf "uv %s\\n" "$*" >> "$LOG"\n'
        'case "$1" in\n'
        '  sync) [[ " $* " != *" --offline "* ]] ;;\n'
        '  export)\n'
        '    if [[ "${MOCK_UV_COLOR:-}" == always && " $* " != *" --color never "* ]]; then\n'
        '      printf "\\033[32m"\n'
        '    fi\n'
        '    echo locked-requirements ;;\n'
        '  pip) [[ "${FAIL_MIRROR:-0}" == 0 ]] ;;\n'
        'esac\n',
        encoding="utf-8",
    )
    python = tools / "python"
    python.write_text(
        '#!/usr/bin/env bash\n'
        'if [[ "$1" != -m ]]; then exec "$REAL_PYTHON" "$@"; fi\n'
        'printf "pip %s\\n" "$*" >> "$LOG"\n'
        'if [[ "${FAIL_DOWNLOAD:-0}" != 0 ]]; then exit 1; fi\n'
        'if [[ $(<"${@: -3:1}") != locked-requirements ]]; then exit 1; fi\n'
        'touch "${!#}/test-locked.whl"\n',
        encoding="utf-8",
    )
    for tool in (uv, python):
        tool.chmod(0o755)
    env = os.environ | {
        "PATH": f"{tools}{os.pathsep}{os.environ['PATH']}",
        "REAL_PYTHON": sys.executable,
        "LOG": str(log),
        "CLUD_LOCAL_UV_CACHE": "true",
        "OBS_RUNNER_CACHE": str(tmp_path),
        "GITHUB_REPOSITORY_ID": "123",
        "RUNNER_TEMP": str(tmp_path),
    }
    return env, log


def test_reuse_wheelhouse_on_another_slot(runner, tmp_path: Path) -> None:
    env, log = runner
    for slot in ("slot-one", "slot-two"):
        process.run(
            ["bash", str(SCRIPT)], cwd=tmp_path, env=env | {"UV_CACHE_DIR": slot}, check=True
        )
    commands = log.read_text(encoding="utf-8").splitlines()
    assert sum(cmd.startswith("pip -m pip download ") for cmd in commands) == 1
    mirror_syncs = [cmd for cmd in commands if cmd.startswith("uv pip sync ")]
    assert len(mirror_syncs) == 2
    assert all("--offline --no-index" in cmd and "--require-hashes" in cmd for cmd in mirror_syncs)


def test_cold_slot_exports_plain_requirements_with_forced_color(runner, tmp_path: Path) -> None:
    env, _ = runner
    result = process.run(
        ["bash", str(SCRIPT)],
        cwd=tmp_path,
        env=env | {"MOCK_UV_COLOR": "always"},
        check=True,
        capture_output=True,
        text=True,
    )
    assert "Installed locked packages from the shared local wheelhouse" in result.stdout
    assert "using regular uv sync" not in result.stdout


def test_failed_download_keeps_regular_sync_available(runner, tmp_path: Path) -> None:
    env, log = runner
    process.run(
        ["bash", str(SCRIPT)], cwd=tmp_path, env=env | {"FAIL_DOWNLOAD": "1"}, check=True
    )
    assert log.read_text(encoding="utf-8").splitlines()[-1].startswith("uv sync --frozen")
    assert not list((tmp_path / "assets").glob("*.tar"))


def test_invalid_archive_is_replaced_under_lock(runner, tmp_path: Path) -> None:
    env, log = runner
    process.run(["bash", str(SCRIPT)], cwd=tmp_path, env=env, check=True)
    asset = next((tmp_path / "assets").glob("*.tar"))
    with tarfile.open(asset, "w") as archive:
        archive.addfile(tarfile.TarInfo("../outside.whl"))
    process.run(["bash", str(SCRIPT)], cwd=tmp_path, env=env, check=True)
    with tarfile.open(asset) as archive:
        assert archive.getnames() == ["test-locked.whl"]
    assert not (tmp_path / "outside.whl").exists()
    assert log.read_text(encoding="utf-8").count("pip -m pip download ") == 2


def test_failed_mirror_install_falls_back_to_regular_sync(runner, tmp_path: Path) -> None:
    env, log = runner
    process.run(
        ["bash", str(SCRIPT)], cwd=tmp_path, env=env | {"FAIL_MIRROR": "1"}, check=True
    )
    commands = log.read_text(encoding="utf-8").splitlines()
    assert any(cmd.startswith("uv pip sync ") for cmd in commands)
    assert commands[-1].startswith("uv sync --frozen")


def test_hosted_sync_uses_no_dell_mirror(runner, tmp_path: Path) -> None:
    env, log = runner
    process.run(
        ["bash", str(SCRIPT)], cwd=tmp_path, env=env | {"CLUD_LOCAL_UV_CACHE": ""}, check=True
    )
    commands = log.read_text(encoding="utf-8").splitlines()
    assert commands == ["uv sync --frozen --group dev --no-install-project --python 3.11"]

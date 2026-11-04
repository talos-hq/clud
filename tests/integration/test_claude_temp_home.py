"""Integration tests: Claude Code installed by clud's temp-home fallback.

On a live USB clud may install Claude Code under the RAM-backed temp dir
(`<tmp>/clud-claude-home-*`). These tests use mock-agent as that install
and check what clud does after the link in `~/.local/bin` is missing, and
after a reboot wiped the temp dir.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

from ._daemon_helpers import copy_launcher, run_clud
from .test_mock_agents import _parse_agent_report, _strip_ansi

pytestmark = [
    pytest.mark.integration,
    pytest.mark.skipif(sys.platform == "win32", reason="the temp-home fallback is POSIX only"),
]

_TIMEOUT = 30


def _env_without_claude(mock_env: dict[str, str], tmp_path: Path) -> dict[str, str]:
    """`mock_env` with no `claude` reachable on PATH and a private temp dir."""
    (tmp_path / "claude").unlink()
    env = dict(mock_env)
    parts = env["PATH"].split(os.pathsep)
    env["PATH"] = os.pathsep.join(p for p in parts if p and not (Path(p) / "claude").exists())
    temp_root = tmp_path / "tmpfs"
    temp_root.mkdir()
    env["TMPDIR"] = str(temp_root)
    return env


def test_claude_in_a_temp_home_is_found_and_logs_move_to_temp(
    clud_binary: Path, mock_agent_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    env = _env_without_claude(mock_env, tmp_path)
    temp_home = Path(env["TMPDIR"]) / "clud-claude-home-test"
    binary = temp_home / ".local" / "bin" / "claude"
    binary.parent.mkdir(parents=True)
    temp_home.chmod(0o700)
    copy_launcher(mock_agent_binary, binary)
    binary.chmod(0o755)

    result = run_clud([str(clud_binary), "-p", "hello"], timeout=_TIMEOUT, env=env)

    output = _strip_ansi(result.stdout + (result.stderr or ""))
    assert result.returncode == 0, output
    report = _parse_agent_report(result)
    assert "clud-claude-home-test" in report["program"], report
    assert "low-space mode" in output, output
    debug = Path(env["HOME"]) / ".claude" / "debug"
    assert debug.is_symlink(), "Claude Code's debug logs should be redirected"
    assert debug.resolve() == (temp_home / "logs" / "claude-debug").resolve()


def test_reboot_wiped_temp_install_reports_claude_missing(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    env = _env_without_claude(mock_env, tmp_path)
    # The link the fallback left; its target vanished with the old tmpfs.
    link = Path(env["HOME"]) / ".local" / "bin" / "claude"
    link.parent.mkdir(parents=True)
    link.symlink_to(Path(env["TMPDIR"]) / "clud-claude-home-gone" / ".local" / "bin" / "claude")

    result = run_clud([str(clud_binary), "-p", "hello"], timeout=_TIMEOUT, env=env)

    output = _strip_ansi(result.stdout + (result.stderr or ""))
    assert result.returncode != 0, output
    assert "claude not found on PATH" in output, output

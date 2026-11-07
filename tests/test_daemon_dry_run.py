"""Daemon dry-run reports intent without touching isolated daemon state (#2678)."""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from tests import process


@pytest.mark.parametrize(
    ("command", "action"),
    [
        ("restart", "stop the running daemon, if any, and start a replacement"),
        ("stop", "stop the running daemon, if any, without starting a replacement"),
        ("api-info", "ensure the daemon is running and report API discovery"),
        ("running-process", "report running-process adoption diagnostics"),
        ("orphan-status", "report the latest orphan-sweep status"),
    ],
)
@pytest.mark.parametrize("position", [1, 2, 3])
def test_daemon_dry_run_reports_intent_without_state_changes(
    tmp_path: Path, command: str, action: str, position: int
) -> None:
    configured = os.environ.get("CLUD_TEST_BINARY")
    binary = (
        Path(configured)
        if configured
        else Path(__file__).resolve().parents[1]
        / "target"
        / "debug"
        / ("clud.exe" if os.name == "nt" else "clud")
    ).resolve()
    assert binary.is_file(), "build the fixed binary or set CLUD_TEST_BINARY"
    state = tmp_path / "state"
    state.mkdir()
    metadata = state / "daemon.json"
    metadata.write_text("isolated daemon sentinel", encoding="utf-8")
    # No process identity, and no possible bringup even if the guard regresses.
    (state / "daemon.lock").mkdir()
    argv = [str(binary), "daemon", command]
    argv.insert(position, "--dry-run")
    env = {
        **os.environ,
        "HOME": str(tmp_path),
        "USERPROFILE": str(tmp_path),
        "CLUD_DAEMON_STATE_DIR": str(state),
    }
    result = process.run(argv, cwd=tmp_path, env=env, capture_output=True, text=True, timeout=30)
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == f"[dry-run] Would {action}. No changes made."
    assert metadata.read_text(encoding="utf-8") == "isolated daemon sentinel"
    assert sorted(path.name for path in state.iterdir()) == ["daemon.json", "daemon.lock"]
    assert [path.name for path in tmp_path.iterdir()] == ["state"]

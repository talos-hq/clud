"""Filesystem temp GC must not start or replace a shared daemon."""

from pathlib import Path

import pytest

from tests.test_hello import _run

pytestmark = pytest.mark.daemon  # Uses the built CLI, even though GC stays local.


def test_gc_tmp_dry_run_needs_no_daemon(tmp_path: Path) -> None:
    state = tmp_path / "state"
    result = _run(
        "--no-daemon",
        "gc",
        "prune",
        "tmp",
        "--dry-run",
        env_overrides={
            "HOME": str(tmp_path),
            "USERPROFILE": str(tmp_path),
            "CLUD_DAEMON_STATE_DIR": str(state),
        },
    )
    assert result.returncode == 0, result.stderr
    assert "within the" in result.stdout
    assert not (state / "daemon.json").exists()

"""Exercise doctor against an isolated stale session registry."""

import json
import os
from pathlib import Path

import pytest

from tests import process

pytestmark = pytest.mark.integration


def test_doctor_reports_a_running_record_without_a_process(
    clud_binary: Path, tmp_path: Path
) -> None:
    sessions = tmp_path / "sessions"
    sessions.mkdir()
    (sessions / "stale.json").write_text(
        json.dumps(
            {
                "id": "stale",
                "kind": "subprocess",
                "daemon_pid": 0,
                "worker_pid": 0xFFFF_FFFF,
                "worker_port": 0,
            }
        ),
        encoding="utf-8",
    )
    result = process.run(
        [str(clud_binary), "doctor", "--json"],
        cwd=tmp_path,
        env={
            **os.environ,
            "CLUD_DAEMON_STATE_DIR": str(tmp_path),
            "CLUD_SESSION_DB": str(tmp_path / "foreground.redb"),
            "CLUD_SESSION_LOCK": str(tmp_path / "foreground.lock"),
        },
        capture_output=True,
        text=True,
        timeout=60,
    )

    assert result.returncode == 1
    assert json.loads(result.stdout)["session_census"]["stale_sessions"] == ["stale"]

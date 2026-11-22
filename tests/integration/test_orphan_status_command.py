"""Integration test for `clud daemon orphan-status` (#465).

The command reads the most recent `orphan_sweep_finished` event straight from
`<state-dir>/daemon-events.jsonl` (no IPC), so it can be exercised end-to-end
with a fabricated event log and no running daemon — deterministic and fast, no
sweep-timing dependence.
"""

from __future__ import annotations

import json
import time
from pathlib import Path

import pytest

from tests import process

from ._daemon_helpers import managed_env

pytestmark = pytest.mark.integration


def _run_orphan_status(
    clud_binary: Path, env: dict[str, str]
) -> process.CompletedProcess[str]:
    return process.run(
        [str(clud_binary), "daemon", "orphan-status", "--json"],
        capture_output=True,
        text=True,
        timeout=30,
        env=env,
    )


def test_orphan_status_reports_recent_sweep_as_fresh(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    state_dir = tmp_path / "daemon-state"
    state_dir.mkdir(parents=True)
    env = managed_env(mock_env, state_dir)

    now_ms = int(time.time() * 1000)
    events = [
        json.dumps({"op": "orphan_sweep_started", "ts_ms": now_ms - 100}),
        json.dumps(
            {"op": "orphan_sweep_finished", "ts_ms": now_ms, "found": 4, "reaped": 2}
        ),
    ]
    (state_dir / "daemon-events.jsonl").write_text("\n".join(events) + "\n")

    result = _run_orphan_status(clud_binary, env)
    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["found"] == 4
    assert payload["reaped"] == 2
    assert payload["last_sweep_ms"] == now_ms
    assert payload["stale"] is False


def test_orphan_status_is_stale_when_no_sweep_recorded(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    state_dir = tmp_path / "daemon-state"
    state_dir.mkdir(parents=True)
    env = managed_env(mock_env, state_dir)
    # Event log exists but contains no finished sweep.
    (state_dir / "daemon-events.jsonl").write_text(
        json.dumps({"op": "some_other_event", "ts_ms": int(time.time() * 1000)}) + "\n"
    )

    result = _run_orphan_status(clud_binary, env)
    payload = json.loads(result.stdout)
    assert payload["last_sweep_ms"] is None
    assert payload["stale"] is True
    assert result.returncode != 0, "stale/no-sweep must exit non-zero"

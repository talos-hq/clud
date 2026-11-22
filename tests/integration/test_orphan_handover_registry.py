"""Integration test for the #465 handover-registry registration.

Creating a detached (daemon-managed) session must record the launching daemon's
PID in `<state-dir>/handover-registry.json` — the persisted fact that lets a
*successor* daemon spare the still-live detached session across a restart
instead of reaping it as a dead-originator orphan.

Uses an isolated state-dir + the mock backend, so it never touches a real
daemon and needs no live-restart of the developer's daemon.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from ._daemon_helpers import (
    daemon_env,
    launch_detached,
    stop_daemon,
    wait_for_file,
)

pytestmark = pytest.mark.integration

def test_detached_session_registers_daemon_pid_in_handover_registry(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    state_dir = tmp_path / "daemon-state"
    env = daemon_env(mock_env, state_dir)
    proc, _session_id = launch_detached(
        clud_binary,
        env,
        "--codex",
        "-p",
        "hello",
        "--",
        "--mock-sleep-ms",
        "3000",
    )
    try:
        registry_path = state_dir / "handover-registry.json"
        # Registration is synchronous with session creation, but the client
        # returns as soon as the daemon replies — give the file a moment.
        wait_for_file(registry_path, timeout=15.0)

        daemon_pid = json.loads((state_dir / "daemon.json").read_text())["pid"]
        registered = json.loads(registry_path.read_text())
        assert isinstance(registered, list)
        assert daemon_pid in registered, (
            f"a detached session must register the daemon pid {daemon_pid} "
            f"for cross-restart sparing; registry held {registered}"
        )
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait(timeout=10)
        stop_daemon(clud_binary, state_dir, env)

"""Integration coverage for the self-expiring test daemon profile (#642)."""

from __future__ import annotations

import json
import shlex
import shutil
import sys
import time
import urllib.request
from pathlib import Path

import pytest

from tests import process

from ._daemon_helpers import (
    daemon_env,
    launch_daemonized,
    launch_detached,
    managed_env,
    process_identity_is_alive,
    session_metadata,
    wait_for_exit,
)

pytestmark = pytest.mark.integration


def _start_daemon(
    clud_binary: Path,
    env: dict[str, str],
    state_dir: Path,
) -> dict[str, object]:
    result = process.run(
        [str(clud_binary), "daemon", "restart"],
        capture_output=True,
        text=True,
        timeout=30,
        env=env,
    )
    assert result.returncode == 0, result.stderr + "\n" + _daemon_diagnostics(state_dir)
    return json.loads((state_dir / "daemon.json").read_text(encoding="utf-8"))


def _wait_for_identity_exit(
    info: dict[str, object],
    timeout: float = 10.0,
    state_dir: Path | None = None,
) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not process_identity_is_alive(
            int(info["pid"]),
            int(info["pid_start"]),
        ):
            return
        time.sleep(0.05)
    details = "" if state_dir is None else "\n" + _daemon_diagnostics(state_dir)
    raise AssertionError(f"daemon identity survived test hard maximum: {info}{details}")


def _daemon_diagnostics(state_dir: Path, session_id: str | None = None) -> str:
    """Everything needed to tell why a daemon is (or is not) still alive (#1375).

    The event log carries `ts_ms` and the exit reason (`daemon_idle_shutdown`,
    `daemon_test_idle_expired`, `daemon_test_max_lifetime_expired`) with the
    worker, lease and connection counts the daemon saw when it decided. A
    liveness assertion that prints this explains itself; a bare
    `assert False` on a PID sends the next reader guessing.
    """
    lines = [f"state_dir: {state_dir}"]
    info_path = state_dir / "daemon.json"
    try:
        lines.append(f"daemon.json: {info_path.read_text(encoding='utf-8')}")
    except OSError as error:
        lines.append(f"daemon.json: <unreadable: {error}>")
    if session_id is not None:
        session_path = state_dir / "sessions" / f"{session_id}.json"
        try:
            lines.append(f"session {session_id}: {session_path.read_text(encoding='utf-8')}")
        except OSError as error:
            lines.append(f"session {session_id}: <unreadable: {error}>")
        log_path = state_dir / "logs" / f"{session_id}.log"
        try:
            lines.append(
                f"session log {session_id} (last 8192 chars): "
                + log_path.read_text(encoding="utf-8", errors="replace")[-8192:]
            )
        except OSError as error:
            lines.append(f"session log {session_id}: <unreadable: {error}>")
    try:
        entries = sorted(
            str(path.relative_to(state_dir)) for path in state_dir.rglob("*")
        )
        lines.append(f"state dir entries: {entries}")
    except OSError as error:
        lines.append(f"state dir entries: <unreadable: {error}>")
    events_path = state_dir / "daemon-events.jsonl"
    try:
        events = events_path.read_text(encoding="utf-8").splitlines()
        lines.append(f"daemon events ({len(events)}):")
        lines.extend(f"  {event}" for event in events)
    except OSError as error:
        lines.append(f"daemon events: <unreadable: {error}>")
    return "\n".join(lines)


def _assert_daemon_alive(
    info: dict[str, object],
    state_dir: Path,
    session_id: str | None = None,
) -> None:
    assert process_identity_is_alive(int(info["pid"]), int(info["pid_start"])), (
        f"daemon {info['pid']} (start {info['pid_start']}) is gone\n"
        + _daemon_diagnostics(state_dir, session_id)
    )


def _poll_dashboard(info: dict[str, object], state_dir: Path) -> None:
    """One authenticated `/state.json` poll that reports the daemon's state on failure."""
    started = time.monotonic()
    try:
        with urllib.request.urlopen(_dashboard_state_request(info), timeout=2) as response:
            assert response.status == 200
            response.read()
    except OSError as error:
        raise AssertionError(
            f"dashboard poll failed after {time.monotonic() - started:.2f}s: "
            f"{type(error).__name__}: {error}\n" + _daemon_diagnostics(state_dir)
        ) from error


def _wait_for_owned_identities(
    state_dir: Path,
    session_id: str,
    timeout: float = 10.0,
) -> list[tuple[int, int]]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        metadata = session_metadata(state_dir, session_id)
        root_pid = metadata.get("root_pid")
        root_start = int(metadata.get("root_pid_start", 0))
        worker_pid = metadata.get("worker_pid")
        worker_start = int(metadata.get("worker_pid_start", 0))
        if (
            root_pid is not None
            and root_start > 0
            and worker_pid is not None
            and worker_start > 0
        ):
            return [
                (int(worker_pid), worker_start),
                (int(root_pid), root_start),
            ]
        time.sleep(0.05)
    raise AssertionError(
        f"worker/backend identities were not recorded for session {session_id}"
    )


def _wait_for_identities_exit(
    identities: list[tuple[int, int]],
    timeout: float = 10.0,
) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not any(
            process_identity_is_alive(pid, start_time)
            for pid, start_time in identities
        ):
            return
        time.sleep(0.05)
    raise AssertionError(f"owned process identities survived daemon exit: {identities}")


def _events(state_dir: Path) -> list[dict[str, object]]:
    path = state_dir / "daemon-events.jsonl"
    if not path.is_file():
        return []
    return [
        json.loads(line)
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def _wait_for_events(
    state_dir: Path,
    expected_ops: set[str],
    timeout: float = 10.0,
) -> set[str]:
    deadline = time.monotonic() + timeout
    ops: set[str] = set()
    while time.monotonic() < deadline:
        try:
            ops = {str(event["op"]) for event in _events(state_dir)}
        except (json.JSONDecodeError, OSError):
            time.sleep(0.05)
            continue
        if expected_ops <= ops:
            return ops
        time.sleep(0.05)
    raise AssertionError(
        f"daemon events did not include {sorted(expected_ops)}: {sorted(ops)}"
    )


def _dashboard_state_request(info: dict[str, object]) -> urllib.request.Request:
    """Build an authenticated request for the daemon's capability dashboard."""
    dashboard_port = info.get("dashboard_port")
    dashboard_token = info.get("dashboard_token")
    assert isinstance(dashboard_port, int), info
    assert isinstance(dashboard_token, str), info
    return urllib.request.Request(
        f"http://127.0.0.1:{dashboard_port}/state.json",
        headers={"Cookie": f"clud_dashboard_token={dashboard_token}"},
    )


def _production_idle_env(
    mock_env: dict[str, str], state_dir: Path, home: Path, timeout_secs: int
) -> dict[str, str]:
    """Isolated production-mode daemon environment with an explicit timeout."""
    env = mock_env.copy()
    for name in (
        "CLUD_DAEMON_TEST_MODE",
        "CLUD_DAEMON_TEST_MAX_LIFETIME_SECS",
        "CLUD_DAEMON_TEST_IDLE_TIMEOUT_SECS",
        "CLUD_DAEMON_TEST_HOST_SCANS",
    ):
        env.pop(name, None)
    env["CLUD_DAEMON_STATE_DIR"] = str(state_dir)
    env["CLUD_EXPERIMENTAL_DAEMON"] = "1"
    env["HOME"] = str(home)
    env["USERPROFILE"] = str(home)
    clud_home = home / ".clud"
    clud_home.mkdir(parents=True)
    (clud_home / "settings.json").write_text(
        json.dumps({"daemon": {"idle_timeout_secs": timeout_secs}}),
        encoding="utf-8",
    )
    return env


def _production_default_idle_env(
    mock_env: dict[str, str], state_dir: Path, home: Path
) -> dict[str, str]:
    """Production profile using the seeded 900-second daemon setting."""
    env = mock_env.copy()
    for name in (
        "CLUD_DAEMON_TEST_MODE",
        "CLUD_DAEMON_TEST_MAX_LIFETIME_SECS",
        "CLUD_DAEMON_TEST_IDLE_TIMEOUT_SECS",
        "CLUD_DAEMON_TEST_HOST_SCANS",
    ):
        env.pop(name, None)
    env["CLUD_DAEMON_STATE_DIR"] = str(state_dir)
    env["CLUD_EXPERIMENTAL_DAEMON"] = "1"
    env["HOME"] = str(home)
    env["USERPROFILE"] = str(home)
    return env


def test_test_daemon_exits_at_hard_max_without_another_request(
    clud_binary: Path,
    mock_env: dict[str, str],
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "daemon-state"
    env = daemon_env(mock_env, state_dir)
    env["CLUD_DAEMON_TEST_MAX_LIFETIME_SECS"] = "4"
    env["CLUD_DAEMON_TEST_IDLE_TIMEOUT_SECS"] = "30"
    proc, session_id = launch_daemonized(
        clud_binary,
        env,
        "--codex",
        "-p",
        "hard-expiry",
        "--",
        "--mock-sleep-ms",
        "30000",
    )
    proc.kill()
    proc.wait(timeout=10)
    owned_identities = _wait_for_owned_identities(state_dir, session_id)
    info = json.loads((state_dir / "daemon.json").read_text(encoding="utf-8"))

    _wait_for_identity_exit(info, timeout=12)
    _wait_for_identities_exit(owned_identities)

    assert not (state_dir / "daemon.json").exists()
    assert "daemon_test_max_lifetime_expired" in {
        event["op"] for event in _events(state_dir)
    }


def _delay_login(env: dict[str, str], tmp_path: Path) -> None:
    # Reproduce initialization exceeding the idle window on Linux too;
    # the hosted macOS login-shell path still needs native CI coverage.
    shell = tmp_path / "slow-login-shell"
    delay_command = shutil.which("sleep")
    assert delay_command is not None
    shell.write_text(
        f'#!/bin/sh\n{shlex.quote(delay_command)} 2.5\nexec /bin/sh "$@"\n',
        encoding="utf-8",
    )
    shell.chmod(0o755)
    env["SHELL"] = str(shell)


@pytest.mark.parametrize("slow_login", [False, True])
def test_test_daemon_exits_after_safe_idle_window(
    clud_binary: Path,
    mock_env: dict[str, str],
    tmp_path: Path,
    slow_login: bool,
) -> None:
    if slow_login and sys.platform == "win32":
        pytest.skip("POSIX login-shell startup regression")
    state_dir = tmp_path / "daemon-state"
    env = managed_env(mock_env, state_dir)
    if slow_login:
        _delay_login(env, tmp_path)
    env["CLUD_DAEMON_TEST_MAX_LIFETIME_SECS"] = "20"
    env["CLUD_DAEMON_TEST_IDLE_TIMEOUT_SECS"] = "2"
    info = _start_daemon(clud_binary, env, state_dir)

    _wait_for_identity_exit(info, state_dir=state_dir)

    events = _events(state_dir)
    started = next(event for event in events if event["op"] == "daemon_started")
    expired = next(event for event in events if event["op"] == "daemon_test_idle_expired")
    if slow_login:
        starting = next(event for event in events if event["op"] == "daemon_starting")
        assert int(started["ts_ms"]) - int(starting["ts_ms"]) >= 2500, events
    assert int(expired["ts_ms"]) - int(started["ts_ms"]) >= 2000, events


def test_configured_production_idle_timeout_exits_and_next_client_restarts(
    clud_binary: Path,
    mock_env: dict[str, str],
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "daemon-state"
    env = _production_idle_env(mock_env, state_dir, tmp_path / "home", 2)
    first = _start_daemon(clud_binary, env, state_dir)

    _wait_for_identity_exit(first, timeout=8)
    assert not (state_dir / "daemon.json").exists()
    shutdown = next(event for event in _events(state_dir) if event["op"] == "daemon_idle_shutdown")
    assert {
        "timeout_secs",
        "idle_ms",
        "worker_count",
        "lease_count",
        "active_connections",
        "active_jobs",
    } <= shutdown.keys()

    second = _start_daemon(clud_binary, env, state_dir)
    assert (int(second["pid"]), int(second["pid_start"])) != (
        int(first["pid"]),
        int(first["pid_start"]),
    )
    stop = process.run(
        [str(clud_binary), "daemon", "stop"],
        capture_output=True,
        text=True,
        timeout=30,
        env=env,
    )
    assert stop.returncode == 0, stop.stderr


def test_production_idle_timeout_defaults_to_fifteen_minutes(
    clud_binary: Path,
    mock_env: dict[str, str],
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "daemon-state"
    home = tmp_path / "home"
    env = _production_default_idle_env(mock_env, state_dir, home)
    _start_daemon(clud_binary, env, state_dir)

    profile = next(
        event for event in _events(state_dir) if event["op"] == "daemon_runtime_profile"
    )
    assert profile["production_idle_timeout_secs"] == 900
    settings = json.loads((home / ".clud" / "settings.json").read_text(encoding="utf-8"))
    assert settings["daemon"]["idle_timeout_secs"] == 900

    stop = process.run(
        [str(clud_binary), "daemon", "stop"],
        capture_output=True,
        text=True,
        timeout=30,
        env=env,
    )
    assert stop.returncode == 0, stop.stderr


@pytest.mark.parametrize("slow_login", [False, True])
def test_dashboard_polling_blocks_configured_production_idle_timeout(
    clud_binary: Path,
    mock_env: dict[str, str],
    tmp_path: Path,
    slow_login: bool,
) -> None:
    if slow_login and sys.platform == "win32":
        pytest.skip("POSIX login-shell startup regression")
    state_dir = tmp_path / "daemon-state"
    env = _production_idle_env(mock_env, state_dir, tmp_path / "home", 2)
    if slow_login:
        _delay_login(env, tmp_path)
    info = _start_daemon(clud_binary, env, state_dir)

    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        _poll_dashboard(info, state_dir)
        _assert_daemon_alive(info, state_dir)
        time.sleep(0.2)

    _wait_for_identity_exit(info, timeout=6, state_dir=state_dir)
    events = _events(state_dir)
    assert "daemon_idle_shutdown" in {event["op"] for event in events}
    if slow_login:
        starting = next(event for event in events if event["op"] == "daemon_starting")
        started = next(event for event in events if event["op"] == "daemon_started")
        assert int(started["ts_ms"]) - int(starting["ts_ms"]) >= 2500, events


def test_detached_worker_blocks_configured_production_idle_timeout(
    clud_binary: Path,
    mock_env: dict[str, str],
    tmp_path: Path,
) -> None:
    """A live detached worker keeps a 2 s-idle daemon up; its exit lets it retire.

    #1375: the worker used to be a fixed `--mock-sleep-ms 4000` and the test
    slept 3 s after the *launcher* exited. The worker's clock starts when the
    daemon spawns it, which is before the launcher's `Create` RPC returns, its
    stderr lines, and its own exit teardown. On a loaded Windows runner that
    stretch exceeded the 1 s margin, the worker finished, the daemon idled out
    exactly as designed, and the assertion blamed the daemon. The worker is
    now held by a release file, so it is provably alive for as long as the
    assertion needs it, and released only afterwards.
    """
    state_dir = tmp_path / "daemon-state"
    env = _production_idle_env(mock_env, state_dir, tmp_path / "home", 2)
    release = tmp_path / "release-worker"
    proc, session_id = launch_detached(
        clud_binary,
        env,
        "--codex",
        # This checks worker liveness, not model discovery. The latter has a
        # bounded multi-second probe that can outlast this 2 s idle window.
        "--model",
        "gpt-6-sol",
        "-p",
        "worker-keeps-daemon-alive",
        "--",
        "--mock-wait-for-file",
        str(release),
    )
    assert wait_for_exit(proc, timeout=10) == 0
    info = json.loads((state_dir / "daemon.json").read_text(encoding="utf-8"))
    owned_identities = _wait_for_owned_identities(state_dir, session_id)

    # Longer than the 2 s idle timeout with no client traffic at all: only the
    # held worker can be keeping the daemon up now.
    time.sleep(3)
    for pid, start_time in owned_identities:
        assert process_identity_is_alive(pid, start_time), (
            f"held worker/backend {pid} exited before release\n"
            + _daemon_diagnostics(state_dir, session_id)
        )
    _assert_daemon_alive(info, state_dir, session_id)

    release.write_text("release", encoding="utf-8")
    _wait_for_identities_exit(owned_identities)
    _wait_for_identity_exit(info, timeout=8, state_dir=state_dir)
    shutdown = next(
        (event for event in _events(state_dir) if event["op"] == "daemon_idle_shutdown"),
        None,
    )
    assert shutdown is not None, _daemon_diagnostics(state_dir, session_id)
    assert shutdown["worker_count"] == 0, _daemon_diagnostics(state_dir, session_id)


def test_foreground_client_lease_blocks_configured_production_idle_timeout(
    clud_binary: Path,
    mock_env: dict[str, str],
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "daemon-state"
    env = _production_idle_env(mock_env, state_dir, tmp_path / "home", 2)
    client = process.Popen(
        [
            str(clud_binary),
            "--codex",
            "--model",
            "gpt-6-sol",
            "--subprocess",
            "-p",
            "foreground-lease-keeps-daemon-alive",
            "--",
            "--mock-sleep-ms",
            "10000",
        ],
        stdout=process.PIPE,
        stderr=process.PIPE,
        text=True,
        env=env,
    )
    # #1637: wait for the lease itself, not merely for daemon.json. The client
    # used to skip the lease on this (centralized) path, so the daemon it
    # started retired during slow Windows startup and `Create` spawned a
    # second one; the test then watched the first, dead, PID.
    _wait_for_events(state_dir, {"client_lease_acquired"}, timeout=8)
    info = json.loads((state_dir / "daemon.json").read_text(encoding="utf-8"))

    # No repeated daemon RPC is sent here. The client lease alone must outlive
    # the two-second idle setting while the foreground process is active.
    time.sleep(3)
    assert process_identity_is_alive(int(info["pid"]), int(info["pid_start"])), (
        _daemon_diagnostics(state_dir)
        + f"\nforeground client exit: {client.poll()}"
        + f"\nforeground stdout: {client.stdout.read() if client.stdout else '<not captured>'}"
        + f"\nforeground stderr: {client.stderr.read() if client.stderr else '<not captured>'}"
    )
    assert wait_for_exit(client, timeout=15) == 0, (
        f"foreground stdout: {client.stdout.read() if client.stdout else '<not captured>'}\n"
        f"foreground stderr: {client.stderr.read() if client.stderr else '<not captured>'}"
    )
    _wait_for_identity_exit(info, timeout=8, state_dir=state_dir)
    events = _events(state_dir)
    assert "daemon_idle_shutdown" in {event["op"] for event in events}
    started = [event for event in events if event["op"] == "daemon_started"]
    assert len(started) == 1, _daemon_diagnostics(state_dir)


def test_zero_production_idle_timeout_remains_disabled(
    clud_binary: Path,
    mock_env: dict[str, str],
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "daemon-state"
    env = _production_idle_env(mock_env, state_dir, tmp_path / "home", 0)
    info = _start_daemon(clud_binary, env, state_dir)

    time.sleep(3)
    _assert_daemon_alive(info, state_dir)
    stop = process.run(
        [str(clud_binary), "daemon", "stop"],
        capture_output=True,
        text=True,
        timeout=30,
        env=env,
    )
    assert stop.returncode == 0, stop.stderr


def test_dashboard_activity_defers_test_idle_expiry(
    clud_binary: Path,
    mock_env: dict[str, str],
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "daemon-state"
    env = managed_env(mock_env, state_dir)
    env["CLUD_DAEMON_TEST_MAX_LIFETIME_SECS"] = "20"
    env["CLUD_DAEMON_TEST_IDLE_TIMEOUT_SECS"] = "2"
    info = _start_daemon(clud_binary, env, state_dir)

    polling_deadline = time.monotonic() + 4
    while time.monotonic() < polling_deadline:
        _poll_dashboard(info, state_dir)
        _assert_daemon_alive(info, state_dir)
        time.sleep(0.25)

    _wait_for_identity_exit(info, timeout=6, state_dir=state_dir)
    assert "daemon_test_idle_expired" in {
        event["op"] for event in _events(state_dir)
    }


def test_test_session_starts_no_host_scanners(
    clud_binary: Path,
    mock_env: dict[str, str],
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "daemon-state"
    env = managed_env(mock_env, state_dir)
    proc, _session_id = launch_detached(
        clud_binary,
        env,
        "--codex",
        "-p",
        "scanner-free",
        "--",
        "--mock-sleep-ms",
        "300",
    )
    assert wait_for_exit(proc, timeout=10) == 0
    events = _events(state_dir)
    profile = next(event for event in events if event["op"] == "daemon_runtime_profile")
    ops = {event["op"] for event in events}

    assert profile["test_mode"] is True
    assert profile["host_scans_enabled"] is False
    assert profile["periodic_maintenance_enabled"] is False
    assert "proc_sampler_started" not in ops
    assert "orphan_sweeper_started" not in ops


def test_test_host_scan_opt_in_starts_scanners(
    clud_binary: Path,
    mock_env: dict[str, str],
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "daemon-state"
    env = managed_env(mock_env, state_dir)
    env["CLUD_DAEMON_TEST_HOST_SCANS"] = "1"
    _start_daemon(clud_binary, env, state_dir)
    ops = _wait_for_events(
        state_dir,
        {"proc_sampler_started", "orphan_sweeper_started"},
    )

    assert "proc_sampler_started" in ops
    assert "orphan_sweeper_started" in ops

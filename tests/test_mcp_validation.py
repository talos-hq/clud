"""Up-front validation of the MCP bridge's tool inputs (#2521).

Every row is one failure class. Each asserts the machine-readable error
(`ok`, `code`, `message`, `fix`, `candidates`) and that the rejection had no
side effect: no clud process was spawned and the daemon API was never
discovered, so no session, worktree or checkout claim can exist.
"""

from __future__ import annotations

import asyncio
import json
import sys

import pytest

from tests.test_mcp_server import _FakeCtx

pytest.importorskip("mcp")


class _Tripwire:
    """Records any attempt to spawn a process or reach the daemon."""

    def __init__(self) -> None:
        self.calls: list[str] = []

    async def spawn(self, *args, **kwargs):
        self.calls.append(f"spawn {args[:2]}")
        raise AssertionError("validation must reject before spawning")

    async def discover(self):
        self.calls.append("discover")
        raise AssertionError("validation must reject before contacting the daemon")


@pytest.fixture
def tripwire(bridge, monkeypatch, tmp_path):
    wire = _Tripwire()
    monkeypatch.setattr(bridge.asyncio, "create_subprocess_exec", wire.spawn)
    monkeypatch.setattr(bridge, "_discover_api", wire.discover)
    # A resolvable clud binary, so only the row under test can fail.
    monkeypatch.setenv("CLUD_BIN", sys.executable)
    return wire


LONG_GOAL = "/goal " + "x" * 4054

# (tool, kwargs, code, text the fix must contain, expected candidate or None)
CASES = [
    (
        "session_start",
        {"prompt": "hi", "backend": "gpt"},
        "invalid_backend",
        'backend="claude"',
        "codex",
    ),
    ("run", {"prompt": "hi", "backend": "gpt"}, "invalid_backend", 'backend="claude"', "claude"),
    ("run_json", {"prompt": "hi", "backend": ""}, "invalid_backend", 'backend="claude"', "codex"),
    ("dry_run", {"prompt": "hi", "backend": "gpt"}, "invalid_backend", 'backend="claude"', "codex"),
    ("session_start", {"prompt": "   "}, "missing_argument", "session_start(prompt=", None),
    ("run", {"prompt": ""}, "missing_argument", "run(prompt=", None),
    ("session_start", {"prompt": "hi", "cwd": "/nope/nope"}, "cwd_not_found", "cwd=", None),
    ("run", {"prompt": "hi", "cwd": "/nope/nope"}, "cwd_not_found", "cwd=", None),
    (
        "session_start",
        {"prompt": "hi", "model": "opus-5.5"},
        "unknown_model",
        'model="claude-opus-5-5"',
        "claude-opus-5-5",
    ),
    (
        "run",
        {"prompt": "hi", "model": "sonnet 5.5"},
        "unknown_model",
        'model="claude-sonnet-5-5"',
        "claude-sonnet-5-5",
    ),
    ("session_start", {"prompt": LONG_GOAL}, "goal_too_long", "4000", None),
    (
        "run",
        {"prompt": "hi", "extra_flags": "do --model x"},
        "misplaced_argument",
        "before the subcommand",
        None,
    ),
    (
        "run",
        {"prompt": "hi", "extra_flags": "--name 'unterminated"},
        "invalid_extra_flags",
        "quote",
        None,
    ),
    (
        "session_start",
        {"prompt": "hi", "wait_seconds": -1},
        "invalid_argument",
        "wait_seconds=0",
        None,
    ),
    ("run", {"prompt": "hi", "timeout": 0}, "invalid_argument", "timeout=", None),
    ("session_status", {"session_id": ""}, "missing_argument", "session_status(session_id=", None),
    ("session_result", {"session_id": "../etc"}, "invalid_session_id", "sess-", None),
    ("session_wait", {"session_id": "a b"}, "invalid_session_id", "sess-", None),
    ("session_kill", {"session_id": ""}, "missing_argument", "session_kill(session_id=", None),
]


@pytest.mark.parametrize(("tool", "kwargs", "code", "fix_part", "candidate"), CASES)
def test_invalid_input_is_rejected_up_front_with_a_correctable_error(
    bridge, tripwire, tool, kwargs, code, fix_part, candidate
):
    raw = asyncio.run(getattr(bridge, tool)(ctx=_FakeCtx(), **kwargs))
    payload = json.loads(raw)
    assert payload["ok"] is False
    assert payload["code"] == code
    assert payload["message"]
    assert fix_part in payload["fix"]
    assert isinstance(payload.get("candidates", []), list)
    if candidate is not None:
        assert candidate in payload["candidates"]
    # Legacy readers keep working.
    assert payload["error"] == payload["message"]
    assert tripwire.calls == [], "rejection had a side effect"


def test_missing_clud_binary_names_the_fix(bridge, tripwire, monkeypatch):
    monkeypatch.setenv("CLUD_BIN", "definitely-not-a-clud-binary-2521")
    payload = json.loads(asyncio.run(bridge.run(prompt="hi", ctx=_FakeCtx())))
    assert payload["code"] == "clud_not_found"
    assert "CLUD_BIN" in payload["fix"]
    assert tripwire.calls == []


def test_unreachable_daemon_is_a_correctable_error(bridge, monkeypatch, tmp_path):
    async def boom():
        raise RuntimeError("connection refused")

    monkeypatch.setattr(bridge, "_discover_api", boom)
    payload = json.loads(asyncio.run(bridge.session_status("sess-1", _FakeCtx())))
    assert payload["ok"] is False
    assert payload["code"] == "daemon_unavailable"
    assert "clud daemon api-info --json" in payload["fix"]


def test_valid_model_ids_and_aliases_pass(bridge):
    from clud import mcp_validate

    for model in ("", "opus", "claude-opus-5-5", "claude-sonnet-5-5[1m]", "gpt-6-astra"):
        assert mcp_validate.check_model(model) is None, model


def test_success_payloads_carry_ok_true(bridge):
    assert json.loads(bridge._dump({"session_id": "s"}))["ok"] is True
    failure = json.loads(bridge._dump({"error": "boom"}))
    assert failure["ok"] is False
    assert failure["code"]
    assert failure["message"] == "boom"
    assert failure["fix"]

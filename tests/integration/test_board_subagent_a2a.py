"""Agent-to-agent bulletin board between a main agent and a codex sub-agent.

The main agent is the real MCP bridge (`src/clud/mcp_server.py`) running with
board id ``main-agent``; it starts a codex sub-agent with ``session_start``.
The real daemon runs the turn with the mock agent standing in for ``codex``;
``MOCK_AGENT_BOARD=1`` makes the mock behave as a board-aware sub-agent (see
``testbins/mock-agent/src/board.rs``). The test proves the round trip the
board exists for: the sub-agent's card is pre-filed with its parent, cwd,
backend and "working on" slot, it reports to its parent, and an instruction
posted by the parent reaches it and is acknowledged.
"""

from __future__ import annotations

import asyncio
import json
import time
from pathlib import Path

import pytest

from tests import process
from tests.test_mcp_server import _FakeCtx

from ._daemon_helpers import daemon_env

pytestmark = pytest.mark.integration

pytest.importorskip("mcp")

PARENT = "main-agent"


def _read_until(bridge, predicate, timeout: float = 60.0) -> list[dict]:
    """board_read (long-polling) as the main agent until ``predicate`` holds."""
    seen: list[dict] = []
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        page = json.loads(asyncio.run(bridge.board_read(wait_seconds=5)))
        seen.extend(page.get("messages") or [])
        if any(predicate(m) for m in seen):
            return seen
    raise AssertionError(f"board never delivered the expected message; saw {seen}")


def _card(bridge, session_id: str) -> dict:
    listing = json.loads(asyncio.run(bridge.board_agents(parent=PARENT)))
    cards = {c["id"]: c for c in listing["agents"]}
    assert session_id in cards, listing
    return cards[session_id]


def test_main_agent_and_codex_subagent_talk_over_the_board(
    bridge, clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, monkeypatch
) -> None:
    state_dir = tmp_path / "daemon-state"
    env = daemon_env(mock_env, state_dir)
    env["MOCK_AGENT_BOARD"] = "1"
    started = process.run(
        [str(clud_binary), "daemon", "restart"],
        capture_output=True,
        text=True,
        timeout=60,
        env=env,
    )
    assert started.returncode == 0, started.stderr
    for key, value in env.items():
        monkeypatch.setenv(key, value)
    monkeypatch.setenv("CLUD_BIN", str(clud_binary))
    monkeypatch.setenv("CLUD_BOARD_ID", PARENT)
    work = tmp_path / "work"
    work.mkdir()

    try:
        started_session = json.loads(
            asyncio.run(
                bridge.session_start("do the subtask", _FakeCtx(), backend="codex", cwd=str(work))
            )
        )
        session_id = started_session["session_id"]

        # The daemon pre-filed the sub-agent's card under its creator.
        card = _card(bridge, session_id)
        assert card["parent"] == PARENT
        assert card["kind"] == "codex"
        assert Path(card["cwd"]).resolve() == work.resolve()

        # Sub-agent -> parent: it reports ready from its own id.
        seen = _read_until(bridge, lambda m: m["body"].startswith("ready:"))
        ready = next(m for m in seen if m["body"].startswith("ready:"))
        assert ready["from"] == session_id
        assert ready["to"] == PARENT
        assert _card(bridge, session_id)["working_on"] == "mock: waiting for instructions"

        # Parent -> sub-agent: an instruction reaches it and is acknowledged.
        posted = json.loads(asyncio.run(bridge.board_post(session_id, "build the widget")))
        assert posted["from"] == PARENT
        _read_until(bridge, lambda m: m["body"] == "ack: build the widget")
        assert _card(bridge, session_id)["working_on"] == "mock: done with build the widget"

        # The main agent's own "working on" slot, through the same directory.
        mine = json.loads(asyncio.run(bridge.board_status("supervising the widget")))
        assert mine["id"] == PARENT
        assert mine["working_on"] == "supervising the widget"
    finally:
        process.run(
            [str(clud_binary), "daemon", "stop"],
            capture_output=True,
            text=True,
            timeout=30,
            env=env,
        )

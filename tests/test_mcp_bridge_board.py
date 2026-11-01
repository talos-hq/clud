"""The bridge's bulletin-board tools against a fake daemon API.

The real board lives in the clud daemon (`crates/clud-bin/src/daemon/board.rs`);
these tests only check that `board_post` / `board_read` send the agent's own id
and that `board_read` resumes from the cursor the previous read returned.
"""

from __future__ import annotations

import asyncio
import json

import pytest

pytest.importorskip("mcp")


@pytest.fixture
def fake_api(bridge, monkeypatch):
    calls: list[tuple[str, str, object]] = []
    board: list[dict] = []

    async def discover():
        return "http://127.0.0.1:1", "tok"

    async def api(base, token, method, path, body=None, headers=None):
        del base, token, headers
        calls.append((method, path, body))
        if method == "POST":
            message = {**body, "cursor": len(board) + 1}
            board.append(message)
            return 201, message
        after = int(path.split("after=")[1])
        page = [m for m in board if m["cursor"] > after]
        return 200, {"messages": page, "next_cursor": len(board)}

    monkeypatch.setattr(bridge, "_discover_api", discover)
    monkeypatch.setattr(bridge, "_api", api)
    monkeypatch.setattr(bridge, "_board_cursor", 0)
    monkeypatch.setenv("CLUD_SESSION_ID", "sess-1")
    monkeypatch.delenv("CLUD_BOARD_ID", raising=False)
    return calls


def test_board_tools_are_discoverable(bridge) -> None:
    async def check() -> None:
        names = {tool.name for tool in await bridge.server.list_tools()}
        assert {"board_post", "board_read", "board_whoami"} <= names

    asyncio.run(check())


def test_post_carries_the_agents_id(bridge, fake_api) -> None:
    out = json.loads(asyncio.run(bridge.board_post("all", "hello")))
    assert out["from"] == "sess-1"
    assert fake_api[0] == (
        "POST",
        "/v1/board/messages",
        {"to": "all", "from": "sess-1", "body": "hello"},
    )


def test_read_resumes_from_the_last_cursor(bridge, fake_api) -> None:
    asyncio.run(bridge.board_post("sess-1", "first"))
    first = json.loads(asyncio.run(bridge.board_read()))
    assert [m["body"] for m in first["messages"]] == ["first"]
    assert "to=sess-1" in fake_api[-1][1]
    asyncio.run(bridge.board_post("sess-1", "second"))
    second = json.loads(asyncio.run(bridge.board_read()))
    assert [m["body"] for m in second["messages"]] == ["second"]


def test_board_id_override_wins(bridge, fake_api, monkeypatch) -> None:
    del fake_api
    monkeypatch.setenv("CLUD_BOARD_ID", "worker-7")
    assert json.loads(asyncio.run(bridge.board_whoami())) == {"ok": True, "id": "worker-7"}

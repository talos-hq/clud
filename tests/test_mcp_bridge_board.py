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
    monkeypatch.setattr(bridge, "_board_first_read_done", False)
    monkeypatch.setenv("CLUD_SESSION_ID", "sess-1")
    monkeypatch.delenv("CLUD_BOARD_ID", raising=False)
    monkeypatch.delenv(bridge.FIRST_READ_BROADCASTS_ENV_VAR, raising=False)
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


def _seed_backlog(bridge, broadcasts: int, direct: int = 0) -> None:
    for i in range(broadcasts):
        asyncio.run(bridge.board_post("all", f"broadcast-{i}"))
    for i in range(direct):
        asyncio.run(bridge.board_post("sess-1", f"direct-{i}"))


def test_first_read_caps_the_broadcast_backlog(bridge, fake_api) -> None:
    del fake_api
    _seed_backlog(bridge, 20, direct=2)
    first = json.loads(asyncio.run(bridge.board_read()))
    bodies = [m["body"] for m in first["messages"]]
    # every message addressed to this agent survives, whatever its age
    assert [b for b in bodies if b.startswith("direct-")] == ["direct-0", "direct-1"]
    # and only the newest 8 broadcasts come with it
    assert [b for b in bodies if b.startswith("broadcast-")] == [
        f"broadcast-{i}" for i in range(12, 20)
    ]
    assert first["skipped_broadcasts"] == 12
    assert "board_read(after=0)" in first["note"]


def test_first_read_page_is_compact(bridge, fake_api) -> None:
    del fake_api
    _seed_backlog(bridge, 3)
    out = asyncio.run(bridge.board_read())
    assert "\n" not in out


def test_capped_first_read_does_not_replay_on_the_next_read(bridge, fake_api) -> None:
    del fake_api
    _seed_backlog(bridge, 20, direct=1)
    asyncio.run(bridge.board_read())
    second = json.loads(asyncio.run(bridge.board_read()))
    assert second["messages"] == []
    assert "skipped_broadcasts" not in second


def test_explicit_after_zero_replays_the_whole_backlog(bridge, fake_api) -> None:
    del fake_api
    _seed_backlog(bridge, 20, direct=2)
    page = json.loads(asyncio.run(bridge.board_read(0)))
    assert len(page["messages"]) == 22
    assert "skipped_broadcasts" not in page


def test_negative_override_restores_the_full_first_read(bridge, fake_api, monkeypatch) -> None:
    del fake_api
    monkeypatch.setenv(bridge.FIRST_READ_BROADCASTS_ENV_VAR, "-1")
    _seed_backlog(bridge, 20)
    page = json.loads(asyncio.run(bridge.board_read()))
    assert len(page["messages"]) == 20
    assert "skipped_broadcasts" not in page


def test_zero_override_withholds_every_broadcast(bridge, fake_api, monkeypatch) -> None:
    del fake_api
    monkeypatch.setenv(bridge.FIRST_READ_BROADCASTS_ENV_VAR, "0")
    _seed_backlog(bridge, 20, direct=1)
    page = json.loads(asyncio.run(bridge.board_read()))
    assert [m["body"] for m in page["messages"]] == ["direct-0"]
    assert page["skipped_broadcasts"] == 20

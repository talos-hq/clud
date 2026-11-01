"""Tests for the async session surface of the bundled MCP bridge.

These drive the bridge against an in-process stand-in for clud's
``/v1/sessions`` daemon API, so no real clud binary, daemon, or agent run is
needed. The fake exposes exactly the contract the bridge relies on: create +
submit a turn (202, immediately), a pollable session record, cursor-paged
events, and a kill.
"""

from __future__ import annotations

import asyncio
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from tests.test_mcp_server import _fake_clud, _FakeCtx


def _run(coro):
    """Run one bridge coroutine to completion."""
    return asyncio.run(coro)


class _DaemonState:
    """Mutable state shared with the fake daemon's request handler."""

    def __init__(self) -> None:
        self.seal_after_polls = 1
        self.polls = 0
        self.requests: list[tuple[str, str, dict, object]] = []
        self.events: list[dict] = []
        #: None = the default (running until `seal_after_polls`), else one of
        #: "idle" / "failed" / "terminated".
        self.terminal_state: str | None = None
        #: Override the recorded turns list (e.g. [] for the submit window).
        self.turns: list[dict] | None = None
        self.base_url = ""


def _make_handler(state: _DaemonState):  # noqa: C901
    class _Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, format, *args) -> None:
            """Silence per-request logging so pytest output stays clean."""

        def _send(self, code: int, payload) -> None:
            body = json.dumps(payload).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _record(self, method: str, body=None) -> str:
            path = self.path
            state.requests.append(
                (method, path, {k.lower(): v for k, v in self.headers.items()}, body)
            )
            return path

        def _record_payload(self, session_id: str) -> dict:
            state.polls += 1
            sealed = state.polls >= state.seal_after_polls
            session_state = state.terminal_state or ("idle" if sealed else "running")
            active = None if (state.terminal_state or sealed) else "turn-1"
            if state.turns is not None:
                turns = list(state.turns)
            else:
                turns = [
                    {
                        "id": "turn-1",
                        "state": "completed" if sealed else "running",
                        "disposition": "exit_0" if sealed else None,
                        "generation": 1,
                        "completed_at_ms": 1 if sealed else None,
                    }
                ]
            return {
                "schema_version": 1,
                "id": session_id,
                "backend": "claude",
                "cwd": "/tmp",
                "state": session_state,
                "generation": 1,
                "current_turn_id": active,
                "created_at_ms": int(time.time() * 1000) - 5000,
                "updated_at_ms": int(time.time() * 1000),
                "last_error": None,
                "turns": turns,
                "events": [],
                "next_event_cursor": len(state.events) + 1,
            }

        def do_GET(self) -> None:
            path = self._record("GET")
            route, _, query = path.partition("?")
            if route.startswith("/v1/sessions/") and route.endswith("/events"):
                after = 0
                limit = 128
                for pair in query.split("&"):
                    key, _, value = pair.partition("=")
                    if key == "after" and value:
                        after = int(value)
                    elif key == "limit" and value:
                        limit = int(value)
                page = [ev for ev in state.events if ev["cursor"] > after][:limit]
                next_cursor = page[-1]["cursor"] if page else after
                return self._send(
                    200,
                    {"events": page, "next_cursor": next_cursor, "retention_gap": False},
                )
            if route.startswith("/v1/sessions/"):
                session_id = route[len("/v1/sessions/") :]
                if session_id == "missing":
                    return self._send(
                        404, {"code": "not_found", "message": "session not found"}
                    )
                return self._send(200, self._record_payload(session_id))
            if route == "/v1/sessions":
                return self._send(200, [])
            return self._send(404, {"code": "not_found", "message": "API route not found"})

        def _read_body(self):
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b""
            if not raw:
                return None
            try:
                return json.loads(raw)
            except json.JSONDecodeError:
                return raw.decode("utf-8", "replace")

        def do_POST(self) -> None:
            body = self._read_body()
            path = self._record("POST", body)
            if path == "/v1/sessions":
                return self._send(
                    201,
                    {
                        "id": "sess-created-1",
                        "state": "starting",
                        "turns": [],
                        "created_at_ms": int(time.time() * 1000),
                    },
                )
            if path.startswith("/v1/sessions/") and path.endswith("/turns"):
                return self._send(
                    202,
                    {
                        "session_id": path[len("/v1/sessions/") : -len("/turns")],
                        "turn_id": "turn-1",
                        "generation": 1,
                        "status": "started",
                    },
                )
            return self._send(404, {"code": "not_found", "message": "API route not found"})

        def do_DELETE(self) -> None:
            path = self._record("DELETE")
            if path == "/v1/sessions/missing":
                return self._send(
                    404, {"code": "not_found", "message": "session not found"}
                )
            return self._send(200, {"status": "terminated"})

    return _Handler


@pytest.fixture
def daemon():
    """A live in-process stand-in for the clud daemon API."""
    state = _DaemonState()
    server = ThreadingHTTPServer(("127.0.0.1", 0), _make_handler(state))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    state.base_url = f"http://127.0.0.1:{server.server_address[1]}"
    try:
        yield state
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


@pytest.fixture
def wired(bridge, daemon, tmp_path, monkeypatch):
    """Point the bridge's `clud daemon api-info` at the fake daemon."""
    doc = json.dumps({"base_url": daemon.base_url, "token": "tok"})
    monkeypatch.setenv("CLUD_BIN", _fake_clud(tmp_path, doc + "\n"))
    return daemon


def _result(raw: str) -> dict:
    return json.loads(raw)


def _event(cursor: int, turn_id: str, text: str) -> dict:
    line = json.dumps(
        {"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}
    )
    return {
        "cursor": cursor,
        "at_ms": cursor,
        "turn_id": turn_id,
        "kind": "raw_jsonl",
        "data": {"line": line},
    }


# --------------------------------------------------------------------------- #
# session_start
# --------------------------------------------------------------------------- #


def test_session_start_returns_handle_immediately_without_waiting(bridge, wired, tmp_path):
    payload = _result(
        _run(bridge.session_start("hello", _FakeCtx(), cwd=str(tmp_path), wait_seconds=0))
    )
    assert payload["session_id"] == "sess-created-1"
    assert payload["turn_id"] == "turn-1"
    assert payload["done"] is False
    assert payload["state"] == "submitted"
    # The turn POST carries an idempotency key so a client retry cannot start a
    # second run.
    turn_posts = [r for r in wired.requests if r[1].endswith("/turns")]
    assert len(turn_posts) == 1
    assert turn_posts[0][2].get("idempotency-key")


def test_session_start_with_wait_returns_the_sealed_answer(bridge, wired, tmp_path):
    wired.events = [_event(1, "turn-1", "the answer")]
    payload = _result(
        _run(bridge.session_start("hello", _FakeCtx(), cwd=str(tmp_path), wait_seconds=30))
    )
    assert payload["done"] is True
    assert payload["text"] == "the answer"
    assert payload["turn"]["disposition"] == "exit_0"


def test_session_start_rejects_unknown_backend_and_empty_prompt(bridge, wired):
    bad_backend = _result(_run(bridge.session_start("hi", _FakeCtx(), backend="gpt")))
    assert "unknown backend" in bad_backend["error"]
    empty = _result(_run(bridge.session_start("   ", _FakeCtx())))
    assert "must not be empty" in empty["error"]


def test_session_start_rejects_a_missing_cwd(bridge, wired):
    payload = _result(_run(bridge.session_start("hi", _FakeCtx(), cwd="/nope/nope")))
    assert "is not a directory" in payload["error"]


def test_session_start_resumes_an_existing_session_without_creating_one(
    bridge, wired, tmp_path
):
    payload = _result(
        _run(
            bridge.session_start(
                "more", _FakeCtx(), cwd=str(tmp_path), resume_session_id="sess-old"
            )
        )
    )
    assert payload["session_id"] == "sess-old"
    assert not [r for r in wired.requests if r[0] == "POST" and r[1] == "/v1/sessions"]


# --------------------------------------------------------------------------- #
# session_status / session_wait
# --------------------------------------------------------------------------- #


def test_session_wait_times_out_cleanly_instead_of_erroring(bridge, wired):
    wired.seal_after_polls = 10_000  # never seals inside the budget
    payload = _result(_run(bridge.session_wait("sess-x", _FakeCtx(), wait_seconds=1)))
    assert payload["done"] is False
    assert payload["session_id"] == "sess-x"
    assert "STILL RUNNING" in payload["note"]
    assert "error" not in payload


def test_session_wait_returns_as_soon_as_the_turn_seals(bridge, wired):
    wired.seal_after_polls = 2
    started = time.monotonic()
    payload = _result(_run(bridge.session_wait("sess-x", _FakeCtx(), wait_seconds=30)))
    elapsed = time.monotonic() - started
    assert payload["done"] is True
    assert elapsed < 5, "a wait must return on completion, not burn its budget"
    assert payload["waited_s"] < 5


def test_an_unrecorded_turn_is_not_reported_done_while_the_session_lives(bridge, wired):
    # A session that is idle with no turn record yet (the submit window) must
    # not be mistaken for a finished run.
    wired.turns = []
    wired.seal_after_polls = 1
    payload = _result(_run(bridge.session_status("sess-x", _FakeCtx())))
    assert payload["state"] == "idle"
    assert payload["done"] is False


def test_a_dead_session_seals_an_unrecorded_turn(bridge, wired):
    wired.turns = []
    wired.terminal_state = "terminated"
    payload = _result(_run(bridge.session_status("sess-x", _FakeCtx())))
    assert payload["state"] == "terminated"
    assert payload["done"] is True


def test_session_status_reports_a_missing_session(bridge, wired):
    payload = _result(_run(bridge.session_status("missing", _FakeCtx())))
    assert "session lookup failed (404)" in payload["error"]
    assert payload["session_id"] == "missing"


# --------------------------------------------------------------------------- #
# session_result
# --------------------------------------------------------------------------- #


def test_session_result_pages_events_with_a_cursor(bridge, wired):
    wired.events = [_event(i + 1, "turn-1", f"line-{i}") for i in range(150)]
    payload = _result(_run(bridge.session_result("sess-x", _FakeCtx(), turn_id="turn-1")))
    assert payload["done"] is True
    assert payload["next_cursor"] == 150
    assert "line-0" in payload["text"]
    assert "line-149" in payload["text"]

    # Continuing from the cursor reads nothing new (no double-counting).
    again = _result(
        _run(
            bridge.session_result(
                "sess-x", _FakeCtx(), turn_id="turn-1", after=payload["next_cursor"]
            )
        )
    )
    assert again["text"] == ""
    assert again["next_cursor"] == 150


def test_session_result_filters_by_turn(bridge, wired):
    wired.events = [_event(1, "turn-9", "other"), _event(2, "turn-1", "mine")]
    payload = _result(_run(bridge.session_result("sess-x", _FakeCtx(), turn_id="turn-1")))
    assert payload["text"] == "mine"


def test_session_result_can_include_the_raw_events(bridge, wired):
    wired.events = [_event(1, "turn-1", "mine")]
    payload = _result(
        _run(bridge.session_result("sess-x", _FakeCtx(), include_events=True))
    )
    assert payload["events"][0]["kind"] == "raw_jsonl"
    assert payload["retention_gap"] is False


# --------------------------------------------------------------------------- #
# session_kill
# --------------------------------------------------------------------------- #


def test_session_kill_reports_termination(bridge, wired):
    payload = _result(_run(bridge.session_kill("sess-x", _FakeCtx())))
    assert payload == {"ok": True, "session_id": "sess-x", "status": "terminated"}
    assert [r for r in wired.requests if r[0] == "DELETE"]


def test_session_kill_surfaces_a_failure_with_the_handle(bridge, wired):
    payload = _result(_run(bridge.session_kill("missing", _FakeCtx())))
    assert "kill failed (404)" in payload["error"]
    assert payload["session_id"] == "missing"


# --------------------------------------------------------------------------- #
# back-compat
# --------------------------------------------------------------------------- #


def test_run_json_keeps_its_blocking_shape(bridge, wired, tmp_path):
    wired.events = [_event(1, "turn-1", "legacy answer")]
    payload = _result(
        _run(bridge.run_json("hello", _FakeCtx(), cwd=str(tmp_path), timeout=10))
    )
    assert payload["session_id"] == "sess-created-1"
    assert payload["turn_id"] == "turn-1"
    assert payload["text"] == "legacy answer"
    assert payload["state"] == "idle"
    assert payload["events"]


def test_run_json_reports_the_handle_when_the_turn_never_finishes(bridge, wired, tmp_path):
    wired.seal_after_polls = 10_000
    raw = _run(bridge.run_json("hello", _FakeCtx(), cwd=str(tmp_path), timeout=1))
    assert raw.startswith("error: turn did not finish")
    assert "session_id=sess-created-1" in raw

"""Push delivery for notify/watch.py: notify/bus.py broker + blocking subscriber."""

from __future__ import annotations

import asyncio
import importlib.util
import socket
import threading
import time
from pathlib import Path

import pytest

TOOL = Path(__file__).resolve().parents[1] / "crates/clud-bin/assets/tools/notify/bus.py"

TRANSPORTS = [
    pytest.param(
        "unix",
        marks=pytest.mark.skipif(not hasattr(socket, "AF_UNIX"), reason="AF_UNIX only"),
    ),
    "tcp",
]


def _load():
    spec = importlib.util.spec_from_file_location("notify_bus", TOOL)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


bus = _load()


@pytest.fixture(params=TRANSPORTS)
def broker(request, tmp_path, monkeypatch):
    monkeypatch.setenv("CLUD_WATCH_BUS", request.param)
    # AF_UNIX paths are length-limited (macOS: 104 bytes); keep it short.
    short = Path("/tmp") / f"bus-{id(tmp_path) % 100000}"
    short.mkdir(exist_ok=True)
    monkeypatch.setenv("CLUD_WATCH_DIR", str(short))
    loop = asyncio.new_event_loop()
    task = loop.create_task(bus.serve(bus.socket_path(), 8))
    thread = threading.Thread(target=loop.run_until_complete, args=(task,), daemon=True)
    thread.start()
    for _ in range(100):
        if bus.socket_path().exists() or bus.addr_path().exists():
            break
        time.sleep(0.02)
    yield short
    loop.call_soon_threadsafe(task.cancel)
    thread.join(2)


def _sub(name, results):
    def run():
        results[name] = bus.subscribe(name)

    t = threading.Thread(target=run)
    t.start()
    return t


def _event(seq, kind="session-sealed"):
    return {"seq": seq, "kind": kind, "subject": "s", "detail": "d", "ok": True}


@pytest.mark.parametrize("transport", TRANSPORTS)
def test_no_broker_exits_3(transport, tmp_path, monkeypatch, capsys):
    monkeypatch.setenv("CLUD_WATCH_BUS", transport)
    monkeypatch.setenv("CLUD_WATCH_DIR", str(tmp_path))
    assert bus.subscribe("x") == 3
    assert "cannot connect" in capsys.readouterr().err
    assert bus.publish_event(_event(1)) is False


def test_fan_out_replay_and_gap(broker, capsys):
    for name in ("a", "b"):
        (broker / f"sub-{name}.cursor").write_text("0")
    results: dict = {}
    threads = [_sub("a", results), _sub("b", results)]
    time.sleep(0.3)
    assert bus.publish_event(_event(1))
    for t in threads:
        t.join(3)
    assert results == {"a": 0, "b": 0}
    assert capsys.readouterr().out.count("#1 session-sealed") == 2

    # down subscriber: published while nobody listened -> replayed on connect
    assert bus.publish_event(_event(2))
    assert bus.subscribe("a") == 0
    assert "#2 session-sealed" in capsys.readouterr().out

    # a miss the broker no longer holds -> explicit gap line
    (broker / "sub-a.cursor").write_text("2")
    assert bus.publish_event(_event(9))
    assert bus.subscribe("a") == 0
    out = capsys.readouterr().out
    assert "gap: missed seq 3..8" in out
    assert "#9" in out


def test_tcp_refuses_missing_or_wrong_token(tmp_path, monkeypatch, capsys):
    monkeypatch.setenv("CLUD_WATCH_BUS", "tcp")
    monkeypatch.setenv("CLUD_WATCH_DIR", str(tmp_path))
    loop = asyncio.new_event_loop()
    task = loop.create_task(bus.serve(bus.socket_path(), 8))
    thread = threading.Thread(target=loop.run_until_complete, args=(task,), daemon=True)
    thread.start()
    for _ in range(100):
        if bus.addr_path().exists():
            break
        time.sleep(0.02)
    try:
        assert bus.addr_path().stat().st_mode & 0o077 == 0
        info = bus._read_addr()
        assert info["host"] == "127.0.0.1"
        for extra, why in (({}, "missing token"), ({"token": "nope"}, "wrong token")):
            with socket.create_connection((info["host"], info["port"]), timeout=2) as sock:
                bus._send(sock, {"op": "sub", "after": 0, **extra})
                reply = sock.makefile("rb").readline()
            assert why in reply.decode()
        good = dict(info)
        bus.addr_path().write_text(__import__("json").dumps({**good, "token": "bad"}))
        assert bus.subscribe("t") == 3
        assert "refused: wrong token" in capsys.readouterr().err
    finally:
        loop.call_soon_threadsafe(task.cancel)
        thread.join(2)

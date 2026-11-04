"""Push delivery for notify/watch.py: notify/bus.py broker + blocking subscriber."""

from __future__ import annotations

import asyncio
import importlib.util
import json
import os
import socket
import threading
import time
from pathlib import Path
from types import SimpleNamespace

import pytest

TOOL = Path(__file__).resolve().parents[1] / "crates/clud-bin/assets/tools/notify/bus.py"


def _load():
    spec = importlib.util.spec_from_file_location("notify_bus", TOOL)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


bus = _load()


@pytest.fixture
def broker(tmp_path, monkeypatch):
    monkeypatch.setenv("CLUD_WATCH_DIR", str(tmp_path))
    loop = asyncio.new_event_loop()
    task = loop.create_task(bus.serve(8))
    thread = threading.Thread(target=loop.run_until_complete, args=(task,), daemon=True)
    thread.start()
    for _ in range(100):
        if bus.addr_path().exists():
            break
        time.sleep(0.02)
    yield tmp_path
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


def test_no_broker_exits_3(tmp_path, monkeypatch, capsys):
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


def test_refuses_missing_or_wrong_token(broker, capsys):
    # Windows stat exposes synthetic mode bits, not its directory/file ACL.
    if os.name != "nt":
        assert bus.addr_path().stat().st_mode & 0o077 == 0
    info = bus._read_addr()
    assert info["host"] == "127.0.0.1"
    assert len(info["token"]) == 64
    for extra, why in (({}, "missing token"), ({"token": "nope"}, "wrong token")):
        with socket.create_connection((info["host"], info["port"]), timeout=2) as sock:
            bus._send(sock, {"op": "sub", "after": 0, **extra})
            reply = sock.makefile("rb").readline()
        assert why in reply.decode()
    bus.addr_path().write_text(json.dumps({**info, "token": "bad"}))
    assert bus.subscribe("t") == 3
    assert "refused: wrong token" in capsys.readouterr().err
    assert bus.publish_event(_event(1)) is False  # refusal surfaced, not swallowed
    assert "refused: wrong token" in capsys.readouterr().err


def test_private_writer_requests_mode_and_replaces_content_atomically(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    destination = tmp_path / "bus.addr"
    destination.write_text("old")
    temporary = destination.with_name(f"bus.addr.{os.getpid()}.tmp")
    operations: list[tuple[str, int | None]] = []

    def record_open(path: Path, flags: int, mode: int) -> int:
        assert path == temporary
        assert flags & os.O_CREAT
        operations.append(("open", mode))
        return os.open(path, flags, mode)

    def record_chmod(path: Path, mode: int) -> None:
        assert destination.read_text() == "old"
        operations.append(("chmod", mode))
        os.chmod(path, mode)

    def record_replace(source: Path, target: Path) -> None:
        assert source == temporary
        assert target == destination
        assert destination.read_text() == "old"
        assert source.read_text() == "new"
        operations.append(("replace", None))
        os.replace(source, target)

    # Patch only this tool's OS binding, preserving real filesystem operations.
    monkeypatch.setattr(
        bus,
        "os",
        SimpleNamespace(
            **{**vars(os), "open": record_open, "chmod": record_chmod, "replace": record_replace}
        ),
    )
    bus._write_private(destination, "new")
    assert operations == [("open", 0o600), ("chmod", 0o600), ("replace", None)]
    assert destination.read_text() == "new"
    assert not temporary.exists()
    if os.name != "nt":
        assert destination.stat().st_mode & 0o077 == 0


def test_no_transport_switch_remains():
    body = TOOL.read_text()
    for gone in ("CLUD_WATCH_BUS", "AF_UNIX", "start_unix_server", "bus.sock"):
        assert gone not in body
    assert '"0.0.0.0"' not in body
    assert 'host="::"' not in body

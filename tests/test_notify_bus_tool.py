"""Push delivery for notify/watch.py: notify/bus.py broker + blocking subscriber."""

from __future__ import annotations

import asyncio
import ctypes
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


@pytest.mark.skipif(os.name == "nt", reason="POSIX creation mode contract")
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


def test_windows_private_writer_fails_closed(tmp_path, monkeypatch):
    destination = tmp_path / "bus.addr"
    destination.write_text("old")

    def fail_setup(_path):
        raise OSError("Win32 security setup failed")

    monkeypatch.setattr(bus, "_open_private_windows", fail_setup, raising=False)
    monkeypatch.setattr(bus, "os", SimpleNamespace(**{**vars(os), "name": "nt"}))
    with pytest.raises(OSError, match="Win32 security setup failed"):
        bus._write_private(destination, "secret")
    assert destination.read_text() == "old"
    assert list(tmp_path.iterdir()) == [destination]


@pytest.mark.skipif(os.name != "nt", reason="native Windows DACL contract")
@pytest.mark.parametrize("override", [False, True])
def test_windows_state_has_protected_owner_only_dacl(tmp_path, monkeypatch, override):
    from ctypes import wintypes as w

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    security = ctypes.WinDLL("advapi32", use_last_error=True)
    bind = bus._win_function
    check = bus._win_check
    free = bind(kernel, "LocalFree", w.LPVOID, w.LPVOID)
    convert = bind(
        security,
        "ConvertStringSecurityDescriptorToSecurityDescriptorW",
        w.BOOL,
        w.LPCWSTR,
        w.DWORD,
        ctypes.POINTER(w.LPVOID),
        ctypes.POINTER(w.DWORD),
    )
    set_security = bind(security, "SetFileSecurityW", w.BOOL, w.LPCWSTR, w.DWORD, w.LPVOID)
    parent_sd = w.LPVOID()
    check(convert("D:P(A;OICI;GA;;;WD)", 1, ctypes.byref(parent_sd), None))
    try:
        # A permissive parent must never leak through, including an explicit override.
        check(set_security(str(tmp_path), 0x80000004, parent_sd))
    finally:
        assert not free(parent_sd)
    if override:
        monkeypatch.setenv("CLUD_WATCH_DIR", str(tmp_path))
    else:
        monkeypatch.delenv("CLUD_WATCH_DIR", raising=False)
        monkeypatch.setattr(bus.Path, "home", lambda: tmp_path)
    destination = bus.addr_path()
    destination.write_text("inherited old state")
    for text in ("secret", "replacement secret"):
        bus._write_private(destination, text)
        assert destination.read_text() == text
        _assert_windows_owner_dacl(destination, kernel, security)


def _assert_windows_owner_dacl(destination, kernel, security):
    from ctypes import wintypes as w

    class Acl(ctypes.Structure):
        _fields_ = [
            ("revision", w.BYTE),
            ("reserved", w.BYTE),
            ("size", w.WORD),
            ("count", w.WORD),
            ("reserved2", w.WORD),
        ]

    class AllowAce(ctypes.Structure):
        _fields_ = [
            ("kind", w.BYTE),
            ("flags", w.BYTE),
            ("size", w.WORD),
            ("mask", w.DWORD),
            ("sid_start", w.DWORD),
        ]

    bind = bus._win_function
    check = bus._win_check
    get_security = bind(
        security,
        "GetNamedSecurityInfoW",
        w.DWORD,
        w.LPCWSTR,
        ctypes.c_int,
        w.DWORD,
        w.LPVOID,
        w.LPVOID,
        ctypes.POINTER(w.LPVOID),
        w.LPVOID,
        ctypes.POINTER(w.LPVOID),
    )
    get_control = bind(
        security,
        "GetSecurityDescriptorControl",
        w.BOOL,
        w.LPVOID,
        ctypes.POINTER(w.WORD),
        ctypes.POINTER(w.DWORD),
    )
    get_ace = bind(security, "GetAce", w.BOOL, w.LPVOID, w.DWORD, ctypes.POINTER(w.LPVOID))
    equal_sid = bind(security, "EqualSid", w.BOOL, w.LPVOID, w.LPVOID)
    free = bind(kernel, "LocalFree", w.LPVOID, w.LPVOID)
    dacl, descriptor = w.LPVOID(), w.LPVOID()
    error = get_security(
        str(destination), 1, 4, None, None, ctypes.byref(dacl), None, ctypes.byref(descriptor)
    )
    assert error == 0
    try:
        control, revision = w.WORD(), w.DWORD()
        check(get_control(descriptor, ctypes.byref(control), ctypes.byref(revision)))
        assert control.value & 0x1000  # SE_DACL_PROTECTED
        assert dacl.value
        assert ctypes.cast(dacl, ctypes.POINTER(Acl)).contents.count == 1
        ace = w.LPVOID()
        check(get_ace(dacl, 0, ctypes.byref(ace)))
        allow = ctypes.cast(ace, ctypes.POINTER(AllowAce)).contents
        assert allow.kind == 0  # ACCESS_ALLOWED_ACE_TYPE
        assert allow.flags == 0  # no inheritance
        assert allow.mask == 0x1F01FF  # file GENERIC_ALL mapped to FILE_ALL_ACCESS
        user = bus._windows_user_sid(kernel, security)
        sid = ctypes.cast(user, ctypes.POINTER(w.LPVOID))[0]
        assert equal_sid(ace.value + AllowAce.sid_start.offset, sid)
    finally:
        assert not free(descriptor)

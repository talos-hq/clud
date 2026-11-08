#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# managed-by: clud
"""notify/bus.py — push delivery for notify/watch.py events (stdlib only).

Transport: one TCP broker on 127.0.0.1, on every platform. clud's
bulletin board is a cursor-read HTTP store with no blocking read, so a
subscriber on it would have to poll; the socket delivers each event the
moment it is published.

  broker [--buffer N]        run the broker (foreground; background it)
  subscribe [--name NAME]    block until the next event, print it, exit 0
  publish KIND SUBJECT DETAIL   hand-publish one event (tests, manual notes)

watch.py publishes every event it records (`publish_event`) right after
appending it to events.jsonl, which stays as the audit trail for `since N`.
With no broker running, publishing is a silent no-op: the event is still
in the log, and the next subscriber reports the gap (below).

No silent loss. Events carry watch.py's monotonic `seq`. The broker keeps
the last --buffer events (default 256) and replays those newer than the
subscriber's cursor on connect. Each subscriber NAME keeps its cursor in
$CLUD_WATCH_DIR/sub-NAME.cursor. When the first event the subscriber sees
is not cursor+1 (the broker restarted, or the miss outran the buffer) it
prints `gap: missed seq A..B; recover with: watch.py since CURSOR` and
still exits 0 — replay when it can, an explicit gap when it cannot, never
nothing. Every subscriber gets every event (fan-out), so two subscribers
with different names both wake.

Subscriber exit contract (one process = one wake-up):
  0  printed one batch (the replayed backlog, or one live event)
  3  cannot connect: broker not running, or it refused the token (stderr)
  4  broker closed the connection before any event arrived
Re-arm: relaunch `subscribe --name NAME` right after it exits; the cursor
file makes the relaunch resume exactly after what it printed.

Security. The broker binds 127.0.0.1 (never 0.0.0.0 or ::) on an
ephemeral port and atomically writes {"host","port","token"} to
$CLUD_WATCH_DIR/bus.addr (mode 0600 on POSIX). Windows creates the file with
a protected DACL granting access only to the current user, including when
CLUD_WATCH_DIR overrides the default directory.
A loopback port is reachable by every
local user, so every request's one handshake line must carry that random
256-bit per-broker token, compared with secrets.compare_digest. A missing
or wrong token is logged by the broker, answered with `{"error": ...}`,
and the connection is closed before any event is sent.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import ctypes
import json
import os
import secrets
import socket
import sys
from collections import deque
from pathlib import Path

DEFAULT_BUFFER = 256
CONNECT_TIMEOUT_S = 2.0


def state_dir() -> Path:
    raw = os.environ.get("CLUD_WATCH_DIR")
    base = Path(raw) if raw else Path.home() / ".clud" / "state" / "watch"
    base.mkdir(parents=True, exist_ok=True)
    return base


def addr_path() -> Path:
    return state_dir() / "bus.addr"


def _win_function(dll, name, result, *args):
    function = getattr(dll, name)
    function.restype = result
    function.argtypes = args
    return function


def _win_check(result):
    if not result:
        raise ctypes.WinError(ctypes.get_last_error())
    return result


def _windows_user_sid(kernel, security):
    """Return TokenUser storage (which owns the SID pointed to by its first field)."""
    from ctypes import wintypes as w

    token = w.HANDLE()
    open_token = _win_function(
        security, "OpenProcessToken", w.BOOL, w.HANDLE, w.DWORD, ctypes.POINTER(w.HANDLE)
    )
    process = _win_function(kernel, "GetCurrentProcess", w.HANDLE)
    close = _win_function(kernel, "CloseHandle", w.BOOL, w.HANDLE)
    info = _win_function(
        security,
        "GetTokenInformation",
        w.BOOL,
        w.HANDLE,
        ctypes.c_int,
        w.LPVOID,
        w.DWORD,
        ctypes.POINTER(w.DWORD),
    )
    _win_check(open_token(process(), 0x0008, ctypes.byref(token)))  # TOKEN_QUERY
    try:
        size = w.DWORD()
        if info(token, 1, None, 0, ctypes.byref(size)) or ctypes.get_last_error() != 122:
            raise ctypes.WinError(ctypes.get_last_error())
        storage = ctypes.create_string_buffer(size.value)
        _win_check(info(token, 1, storage, size, ctypes.byref(size)))  # TokenUser
        return storage
    finally:
        _win_check(close(token))


def _windows_private_descriptor(kernel, security):
    """Build SE_DACL_PROTECTED with exactly one GENERIC_ALL allow ACE."""
    from ctypes import wintypes as w

    user = _windows_user_sid(kernel, security)
    sid = ctypes.cast(user, ctypes.POINTER(w.LPVOID))[0]
    sid_text = w.LPWSTR()
    convert_sid = _win_function(
        security, "ConvertSidToStringSidW", w.BOOL, w.LPVOID, ctypes.POINTER(w.LPWSTR)
    )
    convert_sd = _win_function(
        security,
        "ConvertStringSecurityDescriptorToSecurityDescriptorW",
        w.BOOL,
        w.LPCWSTR,
        w.DWORD,
        ctypes.POINTER(w.LPVOID),
        ctypes.POINTER(w.DWORD),
    )
    free = _win_function(kernel, "LocalFree", w.LPVOID, w.LPVOID)
    _win_check(convert_sid(sid, ctypes.byref(sid_text)))
    try:
        descriptor = w.LPVOID()
        # P sets SE_DACL_PROTECTED; no inherited/group/world ACEs are present.
        _win_check(convert_sd(f"D:P(A;;GA;;;{sid_text.value})", 1, ctypes.byref(descriptor), None))
        return descriptor
    finally:
        if free(ctypes.cast(sid_text, w.LPVOID)):
            raise ctypes.WinError(ctypes.get_last_error())


def _open_private_windows(path: Path) -> int:
    """Exclusively create a private file; never retry with default security."""
    import msvcrt
    from ctypes import wintypes as w

    class SecurityAttributes(ctypes.Structure):
        _fields_ = [("length", w.DWORD), ("descriptor", w.LPVOID), ("inherit", w.BOOL)]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    security = ctypes.WinDLL("advapi32", use_last_error=True)
    descriptor = _windows_private_descriptor(kernel, security)
    create = _win_function(
        kernel,
        "CreateFileW",
        w.HANDLE,
        w.LPCWSTR,
        w.DWORD,
        w.DWORD,
        ctypes.POINTER(SecurityAttributes),
        w.DWORD,
        w.DWORD,
        w.HANDLE,
    )
    free = _win_function(kernel, "LocalFree", w.LPVOID, w.LPVOID)
    close = _win_function(kernel, "CloseHandle", w.BOOL, w.HANDLE)
    attributes = SecurityAttributes(ctypes.sizeof(SecurityAttributes), descriptor, False)
    try:
        handle = create(str(path), 0x40000000, 0, ctypes.byref(attributes), 1, 0x80, None)
        if handle == w.HANDLE(-1).value:  # CREATE_NEW, never open an existing temp
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            return msvcrt.open_osfhandle(handle, os.O_WRONLY)
        except BaseException:
            _win_check(close(handle))
            raise
    finally:
        if free(descriptor):
            raise ctypes.WinError(ctypes.get_last_error())


def _write_private(path: Path, text: str) -> None:
    """Atomic replacement with owner-only access before any content is written."""
    tmp = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    if os.name == "nt":
        fd = _open_private_windows(tmp)
    else:
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as fh:
        fh.write(text)
    if os.name != "nt":
        os.chmod(tmp, 0o600)
    os.replace(tmp, path)


def _read_addr() -> dict:
    info = json.loads(addr_path().read_text())
    return {"host": str(info["host"]), "port": int(info["port"]), "token": str(info["token"])}


def connect(timeout: float | None) -> tuple[socket.socket, str]:
    """Open a client connection to the broker; (sock, token)."""
    try:
        info = _read_addr()
    except (OSError, ValueError, KeyError) as exc:
        raise ConnectionRefusedError(f"no broker address at {addr_path()}: {exc}") from exc
    sock = socket.create_connection((info["host"], info["port"]), timeout=timeout)
    return sock, info["token"]


def _send(sock: socket.socket, message: dict) -> None:
    sock.sendall((json.dumps(message, sort_keys=True) + "\n").encode())


# ------------------------------------------------------------------ broker


class Broker:
    def __init__(self, buffer: int, token: str) -> None:
        self.token = token
        self.recent: deque[dict] = deque(maxlen=buffer)
        self.subscribers: set[asyncio.StreamWriter] = set()

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            line = await reader.readline()
            request = json.loads(line or b"{}")
            if not secrets.compare_digest(str(request.get("token", "")), self.token):
                why = "missing token" if "token" not in request else "wrong token"
                print(f"bus: refused connection: {why}", file=sys.stderr)
                writer.write((json.dumps({"error": f"refused: {why}"}) + "\n").encode())
                await writer.drain()
                writer.close()
                return
            if request.get("op") == "pub":
                await self.publish(request["event"])
            elif request.get("op") == "sub":
                await self.subscribe(int(request.get("after", 0)), reader, writer)
                return
        except (ValueError, KeyError, ConnectionError) as exc:
            print(f"bus: bad request: {exc}", file=sys.stderr)
        writer.close()

    async def publish(self, event: dict) -> None:
        self.recent.append(event)
        payload = (json.dumps({"events": [event]}, sort_keys=True) + "\n").encode()
        for sub in list(self.subscribers):
            try:
                sub.write(payload)
                await sub.drain()
            except ConnectionError:
                self.subscribers.discard(sub)

    async def subscribe(
        self, after: int, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        backlog = [e for e in self.recent if int(e.get("seq", 0)) > after]
        if backlog:
            writer.write((json.dumps({"events": backlog}, sort_keys=True) + "\n").encode())
            await writer.drain()
        self.subscribers.add(writer)
        try:
            await reader.read()  # parks until the subscriber hangs up
        finally:
            self.subscribers.discard(writer)
            writer.close()


async def serve(buffer: int) -> None:
    path = addr_path()
    with contextlib.suppress(OSError, ValueError, KeyError):
        info = _read_addr()
        with socket.create_connection((info["host"], info["port"]), timeout=1):
            raise SystemExit(f"bus: broker already running at {path}")
    token = secrets.token_hex(32)
    broker = Broker(buffer, token)
    server = await asyncio.start_server(broker.handle, host="127.0.0.1", port=0)
    port = server.sockets[0].getsockname()[1]
    _write_private(path, json.dumps({"host": "127.0.0.1", "port": port, "token": token}))
    print(f"bus: listening on 127.0.0.1:{port} (address in {path})", flush=True)
    try:
        async with server:
            await server.serve_forever()
    finally:
        with contextlib.suppress(OSError):
            if _read_addr()["port"] == port:
                path.unlink()


# --------------------------------------------------------------- publisher


def _request(op: dict, timeout: float | None) -> socket.socket:
    sock, token = connect(timeout)
    _send(sock, {**op, "token": token})
    return sock


def publish_event(event: dict) -> bool:
    """Deliver one event. False (no raise) if no broker runs or it refused us."""
    try:
        with _request({"op": "pub", "event": event}, CONNECT_TIMEOUT_S) as sock:
            reply = sock.makefile("rb").readline()  # broker closes on accept
    except OSError:
        return False
    if reply:
        print(f"bus: broker {json.loads(reply).get('error', 'bad reply')}", file=sys.stderr)
        return False
    return True


# -------------------------------------------------------------- subscriber


def format_event(event: dict) -> str:
    ok = "" if "ok" not in event else f" ok={str(event['ok']).lower()}"
    return (
        f"#{event.get('seq')} {event.get('kind')} {event.get('subject')}{ok}: {event.get('detail')}"
    )


def _cursor_file(name: str) -> Path:
    return state_dir() / f"sub-{name}.cursor"


def _read_cursor(name: str) -> int:
    with contextlib.suppress(OSError, ValueError):
        return int(_cursor_file(name).read_text().strip())
    return 0


def subscribe(name: str) -> int:
    cursor = _read_cursor(name)
    try:
        sock = _request({"op": "sub", "after": cursor}, CONNECT_TIMEOUT_S)
    except OSError as exc:
        print(f"bus: cannot connect to broker at {addr_path()}: {exc}", file=sys.stderr)
        print("bus: start it with: bus.py broker", file=sys.stderr)
        return 3
    with sock:
        sock.settimeout(None)
        line = sock.makefile("rb").readline()  # blocks; no timeout, no polling
    if not line:
        print("bus: broker closed the connection before any event", file=sys.stderr)
        return 4
    reply = json.loads(line)
    if "error" in reply:
        print(f"bus: broker {reply['error']}", file=sys.stderr)
        return 3
    events = reply["events"]
    first = int(events[0].get("seq", 0))
    if cursor and first > cursor + 1:
        print(f"gap: missed seq {cursor + 1}..{first - 1}; recover with: watch.py since {cursor}")
    for event in events:
        print(format_event(event))
    _cursor_file(name).write_text(str(max(int(e.get("seq", 0)) for e in events)))
    return 0


# --------------------------------------------------------------------- cli


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("broker")
    p.add_argument("--buffer", type=int, default=DEFAULT_BUFFER)
    p = sub.add_parser("subscribe")
    p.add_argument("--name", default="default", help="cursor identity for replay")
    p = sub.add_parser("publish")
    p.add_argument("kind")
    p.add_argument("subject")
    p.add_argument("detail")
    p.add_argument("--seq", type=int, default=0)
    args = ap.parse_args(argv)
    if args.cmd == "broker":
        with contextlib.suppress(KeyboardInterrupt):
            asyncio.run(serve(args.buffer))
        return 0
    if args.cmd == "subscribe":
        return subscribe(args.name)
    event = {"seq": args.seq, "kind": args.kind, "subject": args.subject, "detail": args.detail}
    if not publish_event(event):
        print(f"bus: event not delivered (broker at {addr_path()})", file=sys.stderr)
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())

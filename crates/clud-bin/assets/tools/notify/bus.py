#!/usr/bin/env python
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# managed-by: clud
"""notify/bus.py — push delivery for notify/watch.py events (stdlib only).

Transport: a Unix-domain-socket broker at $CLUD_WATCH_DIR/bus.sock
(default ~/.clud/state/watch/bus.sock). clud's bulletin board is a
cursor-read HTTP store with no blocking read, so a subscriber on it would
have to poll; the socket delivers each event the moment it is published.

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
  3  cannot connect: broker not running / socket missing (loud on stderr)
  4  broker closed the connection before any event arrived
Re-arm: relaunch `subscribe --name NAME` right after it exits; the cursor
file makes the relaunch resume exactly after what it printed.

Portability: Linux and macOS. Windows CPython has no AF_UNIX socket
support in asyncio, so the broker is unsupported there; `subscribe` exits
3 with a message saying so.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import json
import os
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


def socket_path() -> Path:
    return state_dir() / "bus.sock"


def _send(sock: socket.socket, message: dict) -> None:
    sock.sendall((json.dumps(message, sort_keys=True) + "\n").encode())


# ------------------------------------------------------------------ broker


class Broker:
    def __init__(self, buffer: int) -> None:
        self.recent: deque[dict] = deque(maxlen=buffer)
        self.subscribers: set[asyncio.StreamWriter] = set()

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            line = await reader.readline()
            request = json.loads(line or b"{}")
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


async def serve(path: Path, buffer: int) -> None:
    if path.exists():
        try:  # refuse to steal a live broker's socket
            with socket.socket(socket.AF_UNIX) as probe:
                probe.connect(str(path))
            raise SystemExit(f"bus: broker already running at {path}")
        except ConnectionRefusedError:
            path.unlink()
    broker = Broker(buffer)
    server = await asyncio.start_unix_server(broker.handle, path=str(path))
    os.chmod(path, 0o600)
    print(f"bus: listening on {path}", flush=True)
    async with server:
        await server.serve_forever()


# --------------------------------------------------------------- publisher


def publish_event(event: dict) -> bool:
    """Deliver one event to the broker. False (no raise) if none is running."""
    if not hasattr(socket, "AF_UNIX"):
        return False
    try:
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(CONNECT_TIMEOUT_S)
            sock.connect(str(socket_path()))
            _send(sock, {"op": "pub", "event": event})
        return True
    except OSError:
        return False


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
    if not hasattr(socket, "AF_UNIX"):
        print("bus: AF_UNIX sockets are unavailable on this platform", file=sys.stderr)
        return 3
    path = socket_path()
    cursor = _read_cursor(name)
    sock = socket.socket(socket.AF_UNIX)
    try:
        sock.connect(str(path))
    except OSError as exc:
        print(f"bus: cannot connect to broker at {path}: {exc}", file=sys.stderr)
        print("bus: start it with: bus.py broker", file=sys.stderr)
        return 3
    with sock:
        _send(sock, {"op": "sub", "after": cursor})
        line = sock.makefile("rb").readline()  # blocks; no timeout, no polling
    if not line:
        print("bus: broker closed the connection before any event", file=sys.stderr)
        return 4
    events = json.loads(line)["events"]
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
        if not hasattr(socket, "AF_UNIX"):
            print("bus: unsupported on this platform (no AF_UNIX)", file=sys.stderr)
            return 3
        with contextlib.suppress(KeyboardInterrupt):
            asyncio.run(serve(socket_path(), args.buffer))
        return 0
    if args.cmd == "subscribe":
        return subscribe(args.name)
    event = {"seq": args.seq, "kind": args.kind, "subject": args.subject, "detail": args.detail}
    if not publish_event(event):
        print(f"bus: no broker at {socket_path()}; event not delivered", file=sys.stderr)
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())

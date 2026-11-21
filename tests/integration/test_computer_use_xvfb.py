"""End-to-end computer use under Xvfb (DD-232).

Takes the `cua` MCP server exactly as `clud --dry-run` registers it, starts it
the way Claude Code would, and drives the pinned cua-driver over stdio against
a real X11 window: `tools/list`, `list_windows` and a desktop screenshot.

Opt-in because the first run downloads the pinned driver (~30 MB): set
`CLUD_TEST_CUA_E2E=1` on Linux with `Xvfb` and `xlogo` on PATH.
`CLUD_TEST_CUA_DRIVER` may name an already-downloaded driver to skip the
download (it becomes `CLUD_CUA_DRIVER` for the server). The suite scrubs other
inherited `CLUD_*` names, hence the `CLUD_TEST_` prefix.
"""

from __future__ import annotations

import json
import os
import random
import shutil
import sys
import time
from pathlib import Path

import pytest

from tests.process import DEVNULL, PIPE, Popen, TimeoutExpired, run

pytestmark = pytest.mark.skipif(
    sys.platform != "linux"
    or os.environ.get("CLUD_TEST_CUA_E2E") != "1"
    or not shutil.which("Xvfb")
    or not shutil.which("xlogo"),
    reason="needs Linux, Xvfb, xlogo and CLUD_TEST_CUA_E2E=1",
)

PROTOCOL = "2025-06-18"


def _wait_for(predicate, timeout: float, what: str) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {what}")


def _registered_server(clud: Path, env: dict[str, str]) -> dict:
    result = run(
        [str(clud), "--claude", "--dry-run"],
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
    )
    assert result.returncode == 0, result.stderr
    plan = json.loads(result.stdout)
    command = plan["command"] if "command" in plan else plan["plan"]["command"]
    configs = [arg.split("=", 1)[1] for arg in command if arg.startswith("--mcp-config=")]
    servers = [json.loads(config)["mcpServers"] for config in configs]
    cua = [s["cua"] for s in servers if "cua" in s]
    assert cua, f"no cua server registered: {command}"
    assert "--allowedTools=mcp__cua" in command
    return cua[0]


class _McpClient:
    """A line-oriented MCP client over a FIFO.

    The running-process adapter closes stdin after one write, and an MCP
    session needs many, so the server reads its stdin from a FIFO this
    client keeps open.
    """

    def __init__(self, server: dict, env: dict[str, str], fifo: Path) -> None:
        child_env = dict(env)
        child_env.update(server.get("env", {}))
        os.mkfifo(fifo)
        child_env["CUA_E2E_FIFO"] = str(fifo)
        self.child = Popen(
            ["sh", "-c", 'exec "$0" "$@" <"$CUA_E2E_FIFO"', server["command"], *server["args"]],
            env=child_env,
            stdin=DEVNULL,
            stdout=PIPE,
            stderr=PIPE,
            text=True,
        )
        # Blocks until the shell opens the FIFO for reading.
        self.writer = open(fifo, "w", encoding="utf-8")
        self.next_id = 0

    def _send(self, message: dict) -> None:
        self.writer.write(json.dumps(message) + "\n")
        self.writer.flush()

    def request(self, method: str, params: dict, timeout: float = 240) -> dict:
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                line = self.child.next_stdout(timeout=1.0)
            except TimeoutError:
                line = None
            if not isinstance(line, str) or not line.strip().startswith("{"):
                if self.child.poll() is not None:
                    raise AssertionError(f"server exited {self.child.returncode}")
                continue
            reply = json.loads(line)
            if reply.get("id") == self.next_id:
                assert "error" not in reply, reply
                return reply["result"]
        raise AssertionError(f"no reply to {method}")

    def notify(self, method: str) -> None:
        self._send({"jsonrpc": "2.0", "method": method})

    def close(self) -> None:
        # EOF on stdin is the MCP shutdown signal.
        self.writer.close()
        try:
            self.child.wait(timeout=20)
        except TimeoutExpired:
            self.child.terminate()
            self.child.wait(timeout=10)


@pytest.fixture
def xvfb_display(tmp_path: Path):
    number = random.randint(90, 400)
    server = Popen(
        ["Xvfb", f":{number}", "-screen", "0", "1280x800x24", "-nolisten", "tcp"],
        stdout=DEVNULL,
        stderr=DEVNULL,
    )
    socket = Path(f"/tmp/.X11-unix/X{number}")
    try:
        _wait_for(socket.exists, 20, "Xvfb socket")
        yield f":{number}"
    finally:
        server.terminate()
        server.wait(timeout=10)


def test_registered_cua_server_sees_and_captures_an_x11_window(
    clud_binary: Path, mock_env: dict[str, str], xvfb_display: str, tmp_path: Path
) -> None:
    env = dict(mock_env)
    env["DISPLAY"] = xvfb_display
    for name in ("WAYLAND_DISPLAY", "XAUTHORITY", "XDG_SESSION_TYPE"):
        env.pop(name, None)
    if os.environ.get("CLUD_TEST_CUA_DRIVER"):
        env["CLUD_CUA_DRIVER"] = os.environ["CLUD_TEST_CUA_DRIVER"]
    window = Popen(["xlogo", "-title", "clud-cua-e2e"], env=env, stdout=DEVNULL, stderr=DEVNULL)
    client = None
    try:
        server = _registered_server(clud_binary, env)
        assert server["env"]["CLUD_CUA_PERMISSION_MODE"] == "unrestricted"
        client = _McpClient(server, env, tmp_path / "mcp-stdin")
        init = client.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": {"name": "e2e", "version": "0"},
            },
        )
        assert "tools" in init["capabilities"]
        client.notify("notifications/initialized")

        names = {tool["name"] for tool in client.request("tools/list", {})["tools"]}
        assert {"list_windows", "get_desktop_state", "click", "type_text"} <= names

        found = []

        def window_listed() -> bool:
            result = client.request("tools/call", {"name": "list_windows", "arguments": {}})
            found[:] = result["structuredContent"]["windows"]
            return any(w.get("title") == "clud-cua-e2e" for w in found)

        _wait_for(window_listed, 30, "the xlogo window in list_windows")

        shot = client.request("tools/call", {"name": "get_desktop_state", "arguments": {}})
        images = [c for c in shot["content"] if c.get("type") == "image"]
        assert images, shot
        assert len(images[0]["data"]) > 1000, shot
    finally:
        if client is not None:
            client.close()
        window.terminate()
        window.wait(timeout=10)

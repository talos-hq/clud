"""Unit tests for the bundled MCP bridge (`src/clud/mcp_server.py`).

The ``mcp`` SDK is a base dependency of clud (except on Windows arm64);
these tests skip when it is absent. No real clud binary or daemon is exercised —
tools that spawn clud run against a fake ``CLUD_BIN`` script. The async session
surface is covered by
``tests/test_mcp_bridge_sessions.py`` against a fake daemon API.

The ``bridge`` fixture lives in ``tests/conftest.py`` (shared with the session
tests).
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
from pathlib import Path

import pytest

pytest.importorskip("mcp")

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "src" / "clud" / "mcp_server.py"


class _FakeCtx:
    """Duck-typed MCP context: logging is a no-op, never fatal."""

    async def info(self, message: str) -> None:
        del message


def _fake_clud(tmp_path: Path, stdout_text: str) -> str:
    """A stand-in `clud` that prints `stdout_text` and exits 0.

    Portable on purpose. The previous version wrote a `#!/bin/sh` script and
    chmod'd it: on Windows there is no shebang, `chmod` is a no-op, and
    `CreateProcess` refuses the file with `WinError 193: %1 is not a valid
    Win32 application`. Every test that actually executed the fake died there
    -- invisibly, because the Rust reaper harness (#1124) aborted the Windows
    lane before pytest ever ran.

    The work is done by a Python file so the payload needs no shell quoting;
    the launcher only has to be something each platform will exec.
    """
    impl = tmp_path / "fake_clud_impl.py"
    impl.write_text(
        f"import sys\nsys.stdout.write({stdout_text!r})\n", encoding="utf-8"
    )
    if os.name == "nt":
        # CreateProcess does run `.cmd` (via cmd.exe); it will not run a
        # extension-less script.
        launcher = tmp_path / "fake-clud.cmd"
        launcher.write_text(
            f'@echo off\r\n"{sys.executable}" "{impl}" %*\r\n', encoding="utf-8"
        )
    else:
        launcher = tmp_path / "fake-clud"
        launcher.write_text(
            f'#!/bin/sh\nexec "{sys.executable}" "{impl}" "$@"\n', encoding="utf-8"
        )
        launcher.chmod(0o755)
    return str(launcher)


def test_build_argv_assembles_backend_model_and_flags(bridge, monkeypatch) -> None:
    monkeypatch.delenv("CLUD_BIN", raising=False)
    assert bridge._build_argv("hi", "claude", "", "") == [
        "clud",
        "-p",
        "hi",
        "--claude",
    ]
    assert bridge._build_argv("hi", "codex", "", "") == [
        "clud",
        "-p",
        "hi",
        "--codex",
    ]
    assert bridge._build_argv("hi", "claude", "terra", "--safe") == [
        "clud",
        "-p",
        "hi",
        "--claude",
        "--model",
        "terra",
        "--safe",
    ]
    monkeypatch.setenv("CLUD_BIN", "/opt/clud")
    assert bridge._build_argv("hi", "claude", "", "")[0] == "/opt/clud"


def test_resolve_cwd_expands_and_absolutizes(bridge, monkeypatch, tmp_path: Path) -> None:
    monkeypatch.chdir(tmp_path)
    assert bridge._resolve_cwd("", "/fallback") == "/fallback"
    # `ntpath.expanduser` reads USERPROFILE and ignores HOME, so setting only
    # HOME expanded `~` to the real profile of whoever ran the suite.
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("USERPROFILE", str(tmp_path))
    assert bridge._resolve_cwd("~/work", "") == str(tmp_path / "work")
    assert bridge._resolve_cwd("rel/dir", "") == str(tmp_path / "rel" / "dir")
    assert bridge._resolve_cwd(str(tmp_path), "") == str(tmp_path)


def test_extract_text_joins_result_and_assistant_text(bridge) -> None:
    events = [
        {"kind": "raw_jsonl", "data": {"line": json.dumps({"type": "result", "result": "done"})}},
        {
            "kind": "raw_jsonl",
            "data": {
                "line": json.dumps(
                    {
                        "type": "assistant",
                        "message": {
                            "content": [
                                {"type": "text", "text": "part1"},
                                {"type": "tool_use", "name": "Bash"},
                            ]
                        },
                    }
                )
            },
        },
        {"kind": "raw_jsonl", "data": {"line": "not json"}},
        {"kind": "other", "data": {"line": json.dumps({"type": "result", "result": "skipped"})}},
    ]
    assert bridge._extract_text(events) == "done\npart1"


def test_extract_text_collapses_the_result_echo(bridge) -> None:
    """A headless run repeats its answer in the trailing `result` line."""
    answer = "the final answer"
    events = [
        {
            "kind": "raw_jsonl",
            "data": {
                "line": json.dumps(
                    {
                        "type": "assistant",
                        "message": {"content": [{"type": "text", "text": answer}]},
                    }
                )
            },
        },
        {"kind": "raw_jsonl", "data": {"line": json.dumps({"type": "result", "result": answer})}},
    ]
    assert bridge._extract_text(events) == answer
    # Prose genuinely repeated across turns (separated by other text) survives.
    events.append(
        {
            "kind": "raw_jsonl",
            "data": {
                "line": json.dumps(
                    {
                        "type": "assistant",
                        "message": {"content": [{"type": "text", "text": "middle"}]},
                    }
                )
            },
        }
    )
    events.append(
        {
            "kind": "raw_jsonl",
            "data": {"line": json.dumps({"type": "result", "result": answer})},
        }
    )
    assert bridge._extract_text(events) == f"{answer}\nmiddle\n{answer}"


def test_dry_run_returns_the_launch_plan_json(bridge, tmp_path: Path, monkeypatch) -> None:
    fake = _fake_clud(tmp_path, '{"model_provider":"claude","dry_run":true}\n')
    monkeypatch.setenv("CLUD_BIN", fake)
    result = asyncio.run(
        bridge.dry_run("hello", _FakeCtx(), backend="codex", model="terra")
    )
    plan = json.loads(result)
    assert plan["model_provider"] == "claude"
    assert plan["dry_run"] is True


def test_run_streams_output_and_appends_exit_code(
    bridge, tmp_path: Path, monkeypatch
) -> None:
    fake = _fake_clud(tmp_path, "hello-from-clud\n")
    monkeypatch.setenv("CLUD_BIN", fake)
    result = asyncio.run(bridge.run("hello", _FakeCtx()))
    assert "hello-from-clud" in result
    assert "[clud exited with code 0]" in result


def test_run_drains_output_past_the_log_cap(bridge, tmp_path: Path, monkeypatch) -> None:
    lines = "".join(f"{n:06d} " + "x" * 92 + "\n" for n in range(5000))
    fake = _fake_clud(tmp_path, lines)
    monkeypatch.setenv("CLUD_BIN", fake)
    result = asyncio.run(bridge.run("hello", _FakeCtx(), timeout=60))
    assert "004999 " in result
    assert "[clud exited with code 0]" in result


def test_run_keeps_a_character_split_across_reads(
    bridge, tmp_path: Path, monkeypatch
) -> None:
    fake = _fake_clud(tmp_path, "a" * 8191 + "\u00e9\n")
    monkeypatch.setenv("CLUD_BIN", fake)
    result = asyncio.run(bridge.run("hello", _FakeCtx()))
    assert "\u00e9" in result
    assert "\ufffd" not in result


def test_run_rejects_unknown_backend(
    bridge, tmp_path: Path, monkeypatch
) -> None:
    # Pin CLUD_BIN so the run reaches the backend validation instead of
    # failing earlier on a PATH miss (the exec runners have no `clud` on PATH).
    fake = _fake_clud(tmp_path, "hi\n")
    monkeypatch.setenv("CLUD_BIN", fake)
    result = asyncio.run(bridge.run("hello", _FakeCtx(), backend="gpt"))
    assert json.loads(result)["code"] == "invalid_backend"


def test_discover_api_reads_base_url_and_token(bridge, tmp_path: Path, monkeypatch) -> None:
    fake = _fake_clud(
        tmp_path,
        '{"base_url":"http://127.0.0.1:1","token":"tok"}\n',
    )
    monkeypatch.setenv("CLUD_BIN", fake)
    base, token = asyncio.run(bridge._discover_api())
    assert (base, token) == ("http://127.0.0.1:1", "tok")


def test_serve_tools_are_discoverable(bridge) -> None:
    async def check() -> None:
        tools = {tool.name: tool for tool in await bridge.server.list_tools()}
        assert {"serve_start", "serve_status", "serve_stop"} <= tools.keys()
        assert "Hermes" in tools["serve_start"].description
        assert "backend" in tools["serve_start"].input_schema["properties"]

    asyncio.run(check())


def _spawn_gateway_fixture_as_python(monkeypatch, tmp_path) -> None:
    # A long-lived Windows .cmd wrapper would hide the Python child's lifetime.
    original = asyncio.create_subprocess_exec

    async def spawn(*args, **kwargs):
        return await original(
            sys.executable, str(tmp_path / "fake_clud_impl.py"), *args[1:], **kwargs
        )

    monkeypatch.setattr(asyncio, "create_subprocess_exec", spawn)


def test_serve_lifecycle_returns_only_client_credentials(bridge, monkeypatch, tmp_path) -> None:
    fake = _fake_clud(tmp_path, "")
    impl = tmp_path / "fake_clud_impl.py"
    impl.write_text(
        "import os, signal, sys, time\n"
        "signal.signal(signal.SIGINT, lambda *_: sys.exit(0))\n"
        "assert sys.argv[1:] == ['serve', '--codex', '--port', '0', '--openrouter']\n"
        "assert len(os.environ['CLUD_SERVE_CLIENT_TOKEN']) >= 32\n"
        "print('clud serve listening at http://127.0.0.1:12345', flush=True)\n"
        "while True: time.sleep(1)\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("CLUD_BIN", fake)
    _spawn_gateway_fixture_as_python(monkeypatch, tmp_path)
    monkeypatch.setenv("OPENAI_API_KEY", "upstream-secret-canary")
    monkeypatch.setenv("CLUD_SERVE_CLIENT_TOKEN", "ambient-client-token-must-not-be-reused")

    async def check() -> None:
        async with bridge._gateway_lifespan(bridge.server):
            assert json.loads(await bridge.serve_status()) == {"ok": True, "running": False}
            started = json.loads(await bridge.serve_start(openrouter=True))
            assert started["running"]
            assert started["protocol"] == "anthropic_messages"
            assert started["messages_url"] == "http://127.0.0.1:12345/v1/messages"
            assert len(started["client_token"]) >= 32
            assert "upstream-secret-canary" not in json.dumps(started)
            assert started["client_token"] != os.environ["CLUD_SERVE_CLIENT_TOKEN"]
            assert json.loads(await bridge.serve_status()) == started
            assert json.loads(await bridge.serve_start(openrouter=True)) == started
            assert "error" in json.loads(await bridge.serve_start(backend="claude"))
            child = bridge._gateway._process
        assert child.returncode is not None
        assert json.loads(await bridge.serve_status()) == {"ok": True, "running": False}
        assert json.loads(await bridge.serve_stop()) == {"ok": True, "running": False}

    asyncio.run(check())


def test_serve_start_failures_do_not_echo_provider_output(bridge, monkeypatch, tmp_path) -> None:
    monkeypatch.setenv("CLUD_BIN", _fake_clud(tmp_path, "provider-secret-canary"))

    async def check() -> None:
        result = await bridge.serve_start()
        assert "error" in json.loads(result)
        assert "provider-secret-canary" not in result
        assert json.loads(await bridge.serve_status()) == {"ok": True, "running": False}
        assert "error" in json.loads(await bridge.serve_start(backend="unknown"))
        assert "error" in json.loads(await bridge.serve_start(port=-1))

    asyncio.run(check())


@pytest.mark.parametrize("cancel_start", [False, True])
def test_serve_start_timeout_or_cancellation_reaps_child(
    bridge, monkeypatch, tmp_path, cancel_start
) -> None:
    import clud.serve_mcp as lifecycle

    fake = _fake_clud(tmp_path, "")
    (tmp_path / "fake_clud_impl.py").write_text(
        "import signal, time\n"
        "signal.signal(signal.SIGINT, signal.SIG_IGN)\n"
        "while True: time.sleep(1)\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("CLUD_BIN", fake)
    monkeypatch.setattr(lifecycle, "_STARTUP_TIMEOUT", 0.5)
    monkeypatch.setattr(lifecycle, "_STOP_TIMEOUT", 0.2)
    _spawn_gateway_fixture_as_python(monkeypatch, tmp_path)
    original = asyncio.create_subprocess_exec

    async def check() -> None:
        spawned = asyncio.Event()
        children = []

        async def spawn(*args, **kwargs):
            child = await original(*args, **kwargs)
            children.append(child)
            spawned.set()
            return child

        monkeypatch.setattr(asyncio, "create_subprocess_exec", spawn)
        start = asyncio.create_task(bridge.serve_start())
        await asyncio.wait_for(spawned.wait(), timeout=5)
        if cancel_start:
            start.cancel()
            with pytest.raises(asyncio.CancelledError):
                await start
        else:
            assert "error" in json.loads(await start)
        assert children[0].returncode is not None
        assert json.loads(await bridge.serve_status()) == {"ok": True, "running": False}

    asyncio.run(check())

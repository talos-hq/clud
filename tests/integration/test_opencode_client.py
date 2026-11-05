"""Real OpenCode client acceptance against clud serve and a mock Messages API.

Set CLUD_TEST_OPENCODE_BINARY to the reviewed OpenCode executable. This opt-in
lane never contacts a model provider and uses running-process for every child.
"""

from __future__ import annotations

import http.client
import http.server
import json
import os
import signal
import threading
from contextlib import ExitStack, contextmanager
from pathlib import Path
from urllib.parse import urlsplit

import pytest

from tests import process
from tests.metered_consent import consent_for

pytestmark = pytest.mark.integration


def _events(block: dict, reason: str, model: str) -> bytes:
    if block["type"] == "tool_use":
        start = {**block, "input": {}}
        delta = {"type": "input_json_delta", "partial_json": json.dumps(block["input"])}
    else:
        start = {**block, "text": ""}
        delta = {"type": "text_delta", "text": block["text"]}
    events = [
        (
            "message_start",
            {
                "message": {
                    "id": "msg_mock",
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": None,
                    "usage": {"input_tokens": 10, "output_tokens": 0},
                }
            },
        ),
        ("content_block_start", {"index": 0, "content_block": start}),
        ("content_block_delta", {"index": 0, "delta": delta}),
        ("content_block_stop", {"index": 0}),
        (
            "message_delta",
            {
                "delta": {"stop_reason": reason, "stop_sequence": None},
                "usage": {"output_tokens": 10},
            },
        ),
        ("message_stop", {}),
    ]
    return "".join(
        f"event: {kind}\ndata: {json.dumps({'type': kind, **body})}\n\n" for kind, body in events
    ).encode()


@pytest.fixture
def opencode_mock(tmp_path: Path):
    target = tmp_path / "read-me.txt"
    target.write_text("OPENCODE_FILE_CANARY_7251", encoding="utf-8")
    requests: list[tuple[dict, dict]] = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_POST(self) -> None:
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append((dict(self.headers), body))
            has_result = any(
                block.get("type") == "tool_result"
                for message in body.get("messages", [])
                for block in message.get("content", [])
                if isinstance(block, dict)
            )
            can_read = any(tool.get("name") == "read" for tool in body.get("tools", []))
            if can_read and not has_result:
                block = {
                    "type": "tool_use",
                    "id": "read_canary",
                    "name": "read",
                    "input": {"filePath": str(target)},
                }
                payload = _events(block, "tool_use", body["model"])
            else:
                payload = _events(
                    {"type": "text", "text": "OPENCODE_E2E_FINISHED"}, "end_turn", body["model"]
                )
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"https://127.0.0.1:{server.server_port}", requests
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def _environment(tmp_path: Path, upstream: str) -> dict[str, str]:
    env = os.environ.copy()
    isolated = tmp_path / "home"
    isolated.mkdir()
    vault = isolated / "vault"
    vault.mkdir()
    (vault / "clud_deepseek--api_key_v1.secret").write_text(
        "upstream-canary-7251", encoding="utf-8"
    )
    env.update(
        {
            "HOME": str(isolated),
            "USERPROFILE": str(isolated),
            "XDG_CONFIG_HOME": str(isolated / "config"),
            "XDG_DATA_HOME": str(isolated / "data"),
            "XDG_CACHE_HOME": str(isolated / "cache"),
            "XDG_STATE_HOME": str(isolated / "state"),
            "CLUD_HOOK_HOME": str(isolated),
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_NO_UNLOCK": "1",
            "SOLDR_BROKER_AUTOSPAWN": "0",
            "CLUD_TEST_SECRET_STORE_DIR": str(vault),
            "CLUD_SERVE_CLIENT_TOKEN": "opencode-client-token-7251-abcdefgh",
            "ANTHROPIC_API_KEY": "upstream-canary-7251",
            "CLUD_TEST_GATEWAY_PLAINTEXT_TLS": "1",
            "CLUD_TEST_UNIFIED_ANTHROPIC_UPSTREAM_URL": upstream,
            "CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL": upstream,
            "CLUD_TEST_UNIFIED_DEEPSEEK_UPSTREAM_URL": upstream,
            "CLUD_TEST_UNIFIED_OPENROUTER_UPSTREAM_URL": upstream,
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
            "OPENCODE_DISABLE_MODELS_FETCH": "1",
            "OPENCODE_DISABLE_AUTOUPDATE": "1",
        }
    )
    for name in ("DEEPSEEK_API_KEY", "ANTHROPIC_AUTH_TOKEN"):
        env.pop(name, None)
    return env


@contextmanager
def _gateway(clud_binary: Path, env: dict[str, str], provider: str):
    child = process.Popen(
        [str(clud_binary), "serve", provider, "--port", "0"],
        env=consent_for(env, [provider]),
        stdout=process.PIPE,
        stderr=process.PIPE,
        text=True,
        creationflags=process.CREATE_NEW_PROCESS_GROUP,
    )
    try:
        line = child.next_stdout(timeout=60)
        assert isinstance(line, str), line
        assert "listening at " in line, line
        yield line.strip().split("listening at ", 1)[1]
    finally:
        child.send_signal(signal.SIGINT)
        try:
            child.wait(timeout=15)
        except process.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)


def _client_command(
    clud_binary: Path,
    executable: str,
    launcher: str,
    provider: str,
    model: str,
    base: str,
    env: dict[str, str],
) -> list[str]:
    env["OPENCODE_CONFIG_CONTENT"] = json.dumps(
        {
            "$schema": "https://opencode.ai/config.json",
            "autoupdate": False,
            "model": f"clud/{model}",
            "small_model": f"clud/{model}",
            "provider": {
                "clud": {
                    "npm": "@ai-sdk/anthropic",
                    "name": "clud mock",
                    "options": {"baseURL": base + "/v1", "apiKey": env["CLUD_SERVE_CLIENT_TOKEN"]},
                    "models": {
                        model: {"name": "Mock", "limit": {"context": 200000, "output": 4096}}
                    },
                }
            },
        }
    )
    if launcher == "clud-clean":
        env.pop("OPENCODE_CONFIG_CONTENT")
    prompt = "Read the requested file using the read tool and report the result."
    if launcher == "opencode":
        return [executable, "run", "--pure", "--auto", "--format", "json", prompt]
    env["PATH"] = str(Path(executable).parent) + os.pathsep + env["PATH"]
    env["ANTHROPIC_BASE_URL"] = base
    env["ANTHROPIC_API_KEY"] = env["CLUD_SERVE_CLIENT_TOKEN"]
    env["DEEPSEEK_API_KEY"] = env["CLUD_SERVE_CLIENT_TOKEN"]
    return [
        str(clud_binary),
        provider,
        "--harness",
        "opencode",
        "--model",
        model,
        "--subprocess",
        "-p",
        prompt,
        "--",
        "--pure",
    ]


def _assert_tool_cycle(result, requests: list, model: str) -> None:
    assert result.returncode == 0, result.stdout + result.stderr
    assert "OPENCODE_E2E_FINISHED" in result.stdout, result.stdout + result.stderr
    tool_results = [
        block
        for _, body in requests
        for message in body.get("messages", [])
        for block in message.get("content", [])
        if isinstance(block, dict) and block.get("type") == "tool_result"
    ]
    assert any("OPENCODE_FILE_CANARY_7251" in json.dumps(block) for block in tool_results)
    assert len(requests) >= 2
    assert all(
        body["model"].removesuffix("[1m]") == model.removesuffix("[1m]") for _, body in requests
    )
    assert all("opencode-client-token" not in json.dumps(headers) for headers, _ in requests)
    assert all("outer-client-token" not in json.dumps(headers) for headers, _ in requests)
    assert all("upstream-canary-7251" in json.dumps(headers) for headers, _ in requests)


@pytest.mark.parametrize(
    ("launcher", "provider", "hops"),
    [
        ("opencode", "--claude", 1),
        ("clud", "--claude", 1),
        ("clud", "--deepseek", 1),
        ("clud", "--deepseek", 2),
        ("clud-clean", "--claude", 1),
    ],
)
def test_real_opencode_completes_a_served_tool_cycle(
    clud_binary: Path, tmp_path: Path, opencode_mock, launcher: str, provider: str, hops: int
) -> None:
    executable = os.environ.get("CLUD_TEST_OPENCODE_BINARY")
    if not executable:
        pytest.skip("set CLUD_TEST_OPENCODE_BINARY to run the real-client acceptance lane")
    upstream, requests = opencode_mock
    env = _environment(tmp_path, upstream)
    model = "claude-test" if provider == "--claude" else "deepseek-flash[1m]"
    if launcher == "clud-clean":
        model = "claude-sonnet-5-5"
    with ExitStack() as stack:
        base = stack.enter_context(_gateway(clud_binary, env, provider))
        if hops == 2:
            outer_env = env.copy()
            outer_vault = tmp_path / "outer-vault"
            outer_vault.mkdir()
            (outer_vault / "clud_deepseek--api_key_v1.secret").write_text(
                env["CLUD_SERVE_CLIENT_TOKEN"], encoding="utf-8"
            )
            outer_env["CLUD_TEST_SECRET_STORE_DIR"] = str(outer_vault)
            outer_env["CLUD_SERVE_CLIENT_TOKEN"] = "outer-client-token-7251-abcdefghij"
            outer_env["CLUD_TEST_UNIFIED_DEEPSEEK_UPSTREAM_URL"] = base.replace(
                "http://", "https://"
            )
            base = stack.enter_context(_gateway(clud_binary, outer_env, provider))
            env["CLUD_SERVE_CLIENT_TOKEN"] = outer_env["CLUD_SERVE_CLIENT_TOKEN"]
        address = urlsplit(base)
        connection = http.client.HTTPConnection(address.hostname, address.port, timeout=10)
        connection.request("GET", "/v1/models", headers={"x-api-key": "invalid"})
        assert connection.getresponse().status == 401
        connection.close()
        assert not requests
        command = _client_command(clud_binary, executable, launcher, provider, model, base, env)
        result = process.run(
            command,
            env=consent_for(env, [provider]),
            cwd=tmp_path,
            capture_output=True,
            text=True,
            timeout=120,
        )
        _assert_tool_cycle(result, requests, model)

"""`POST /v1/images/generations` on the durable `clud serve` endpoint.

CLIProxyAPI answers OpenAI's images API from its Codex executor
(``internal/runtime/executor/codex_openai_images.go``); clud had no image door.
These are subprocess tests against a real ``clud serve --codex`` process and a
fake images upstream, so they prove the route, the body rewrite and the
credential boundary rather than the unit-level validator alone.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from tests import process

pytestmark = pytest.mark.integration

_IMAGE_PAYLOAD = {
    "created": int(1e9),
    "data": [{"b64_json": "aW1hZ2UtYnl0ZXM=", "revised_prompt": "a red cube"}],
}


class _FakeImagesServer:
    """A Codex images endpoint that records every request it receives."""

    def __init__(self) -> None:
        self.requests: list[tuple[str, dict[str, str], bytes]] = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: object) -> None:
                pass

            def do_POST(self) -> None:
                size = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(size)
                owner.requests.append((self.path, dict(self.headers), body))
                payload = json.dumps(_IMAGE_PAYLOAD).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.base_url = f"http://127.0.0.1:{self._server.server_port}"
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

    def close(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)


def _call(address, path: str, payload: dict, token: str):
    import http.client

    connection = http.client.HTTPConnection(address.hostname, address.port, timeout=30)
    connection.request(
        "POST",
        path,
        body=json.dumps(payload),
        headers={"x-api-key": token, "content-type": "application/json"},
    )
    response = connection.getresponse()
    result = response.status, response.read()
    connection.close()
    return result


def test_serve_codex_answers_images_generations(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    from urllib.parse import urlsplit

    upstream = _FakeImagesServer()
    token = "images-client-token-0123456789abcd"
    env = mock_env.copy()
    env.update(
        {
            "HOME": str(tmp_path),
            "USERPROFILE": str(tmp_path),
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_SERVE_CLIENT_TOKEN": token,
            "OPENAI_API_KEY": "codex-api-key-canary",
            "CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL": upstream.base_url,
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
        }
    )
    env.pop("ANTHROPIC_API_KEY", None)
    child = process.Popen(
        [str(clud_binary), "serve", "--codex", "--port", "0"],
        env=env,
        stdout=process.PIPE,
        stderr=process.PIPE,
        text=True,
        creationflags=process.CREATE_NEW_PROCESS_GROUP,
    )
    try:
        line = child.next_stdout(timeout=60)
        assert isinstance(line, str), (line, child.poll())
        assert "listening at " in line, (line, child.poll())
        address = urlsplit(line.strip().split("listening at ", 1)[1])

        payload = {
            "model": "gpt-image-1.5",
            "prompt": "a red cube on a white table",
            "size": "1024x1024",
            "n": 1,
        }
        # The client token is the only credential this door accepts.
        assert _call(address, "/v1/images/generations", payload, "wrong")[0] == 401
        assert not upstream.requests

        status, body = _call(address, "/v1/images/generations", payload, token)
        assert status == 200, body
        # The upstream envelope reaches the client byte-for-byte.
        assert json.loads(body) == _IMAGE_PAYLOAD
        assert len(upstream.requests) == 1
        path, headers, raw = upstream.requests[0]
        assert path.endswith("/images/generations"), path
        forwarded = json.loads(raw)
        assert forwarded["model"] == "gpt-image-1.5"
        assert forwarded["prompt"] == payload["prompt"]
        assert forwarded["size"] == "1024x1024"
        assert "stream" not in forwarded
        # The client's token never becomes the upstream bearer.
        assert token not in headers.get("Authorization", "")
        assert token.encode() not in raw
        assert headers.get("Authorization")
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=15)
        upstream.close()


def test_serve_images_refuses_an_unknown_model_without_spending(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    from urllib.parse import urlsplit

    upstream = _FakeImagesServer()
    token = "images-client-token-0123456789abcd"
    env = mock_env.copy()
    env.update(
        {
            "HOME": str(tmp_path),
            "USERPROFILE": str(tmp_path),
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_SERVE_CLIENT_TOKEN": token,
            "OPENAI_API_KEY": "codex-api-key-canary",
            "CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL": upstream.base_url,
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
        }
    )
    env.pop("ANTHROPIC_API_KEY", None)
    child = process.Popen(
        [str(clud_binary), "serve", "--codex", "--port", "0"],
        env=env,
        stdout=process.PIPE,
        stderr=process.PIPE,
        text=True,
        creationflags=process.CREATE_NEW_PROCESS_GROUP,
    )
    try:
        line = child.next_stdout(timeout=60)
        assert isinstance(line, str), (line, child.poll())
        assert "listening at " in line, (line, child.poll())
        address = urlsplit(line.strip().split("listening at ", 1)[1])

        status, body = _call(
            address,
            "/v1/images/generations",
            {"model": "dall-e-3", "prompt": "a cat"},
            token,
        )
        assert status == 400, body
        envelope = json.loads(body)
        assert envelope["error"]["type"] == "invalid_request_error"
        assert "dall-e-3" in envelope["error"]["message"]
        # A refused request costs no upstream call.
        assert not upstream.requests

        status, body = _call(
            address,
            "/v1/images/generations",
            {"model": "gpt-image-1.5", "prompt": "a cat", "stream": True},
            token,
        )
        assert status == 400, body
        assert "streaming" in json.loads(body)["error"]["message"].lower()
        assert not upstream.requests
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=15)
        upstream.close()


def test_serve_without_the_codex_route_refuses_images_by_name(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    from urllib.parse import urlsplit

    token = "images-client-token-0123456789abcd"
    env = mock_env.copy()
    env.update(
        {
            "HOME": str(tmp_path),
            "USERPROFILE": str(tmp_path),
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_SERVE_CLIENT_TOKEN": token,
            "ANTHROPIC_API_KEY": "claude-upstream-canary",
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
        }
    )
    child = process.Popen(
        [str(clud_binary), "serve", "--claude", "--port", "0"],
        env=env,
        stdout=process.PIPE,
        stderr=process.PIPE,
        text=True,
        creationflags=process.CREATE_NEW_PROCESS_GROUP,
    )
    try:
        line = child.next_stdout(timeout=60)
        assert isinstance(line, str), (line, child.poll())
        assert "listening at " in line, (line, child.poll())
        address = urlsplit(line.strip().split("listening at ", 1)[1])
        status, body = _call(
            address,
            "/v1/images/generations",
            {"model": "gpt-image-1.5", "prompt": "a cat"},
            token,
        )
        assert status == 501, body
        message = json.loads(body)["error"]["message"]
        assert "--codex" in message
        assert "claude-upstream-canary" not in body.decode()
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=15)

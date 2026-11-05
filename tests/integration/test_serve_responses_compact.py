"""`POST /v1/responses/compact` on the durable `clud serve` endpoint.

CLIProxyAPI serves the Responses compaction door (``internal/api/server_routes.go:88``)
by forwarding the body to ``<codex base>/responses/compact`` with the
``responses/compact`` action, refusing ``stream: true`` and relaying the answer
verbatim (``openai_responses_handlers.go:917`` ``OpenAIResponsesAPIHandler.Compact``).
clud only called that endpoint for its own auto-compaction, so a served client's
``responses.compact()`` fell through to 404. These are subprocess tests against a
real ``clud serve --codex`` process and a fake Codex upstream, so they prove the
route, the body contract and the credential boundary rather than the unit-level
sanitizer alone.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from tests import process

pytestmark = pytest.mark.integration

_COMPACT_PAYLOAD = {
    "output": [
        {
            "type": "compaction",
            "id": "cmp_1",
            "encrypted_content": "opaque-summary",
        }
    ],
    "usage": {"input_tokens": 42, "output_tokens": 7},
}


class _FakeCompactServer:
    """A Codex compaction endpoint that records every request it receives."""

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
                payload = json.dumps(_COMPACT_PAYLOAD).encode()
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


def _serve(clud_binary: Path, env: dict[str, str], *flags: str):
    return process.Popen(
        [str(clud_binary), "serve", *flags, "--port", "0"],
        env=env,
        stdout=process.PIPE,
        stderr=process.PIPE,
        text=True,
        creationflags=process.CREATE_NEW_PROCESS_GROUP,
    )


def test_serve_codex_answers_responses_compact(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    from urllib.parse import urlsplit

    upstream = _FakeCompactServer()
    token = "compact-client-token-0123456789abcd"
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
    child = _serve(clud_binary, env, "--codex")
    try:
        line = child.next_stdout(timeout=60)
        assert isinstance(line, str), (line, child.poll())
        assert "listening at " in line, (line, child.poll())
        address = urlsplit(line.strip().split("listening at ", 1)[1])

        payload = {
            "model": "gpt-5.1-codex",
            "input": [{"type": "message", "role": "user", "content": "compact me"}],
            "include": ["reasoning.encrypted_content"],
            "stream": False,
        }
        # The client token is the only credential this door accepts.
        assert _call(address, "/v1/responses/compact", payload, "wrong")[0] == 401
        assert not upstream.requests

        status, body = _call(address, "/v1/responses/compact", payload, token)
        assert status == 200, body
        # The upstream answer reaches the client byte-for-byte, including the
        # fields clud's own typed compaction view does not model.
        assert json.loads(body) == _COMPACT_PAYLOAD
        assert len(upstream.requests) == 1
        path, headers, raw = upstream.requests[0]
        assert path.endswith("/responses/compact"), path
        forwarded = json.loads(raw)
        assert forwarded["model"] == "gpt-5.1-codex"
        assert "stream" not in forwarded, forwarded
        assert forwarded["include"] == ["reasoning.encrypted_content"]
        # The client's token never becomes the upstream bearer.
        assert token not in headers.get("Authorization", "")
        assert token.encode() not in raw
        assert headers.get("Authorization")

        # A streaming compaction is refused locally and never spent upstream.
        streaming = dict(payload, stream=True)
        status, body = _call(address, "/v1/responses/compact", streaming, token)
        assert status == 400, body
        assert "Streaming not supported for compact responses" in json.loads(body)["error"][
            "message"
        ]
        assert len(upstream.requests) == 1
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=15)
        upstream.close()


def test_serve_without_the_codex_route_refuses_responses_compact_by_name(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    from urllib.parse import urlsplit

    token = "compact-client-token-0123456789abcd"
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
    child = _serve(clud_binary, env, "--claude")
    try:
        line = child.next_stdout(timeout=60)
        assert isinstance(line, str), (line, child.poll())
        assert "listening at " in line, (line, child.poll())
        address = urlsplit(line.strip().split("listening at ", 1)[1])
        status, body = _call(
            address,
            "/v1/responses/compact",
            {"model": "gpt-5.1-codex", "input": []},
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

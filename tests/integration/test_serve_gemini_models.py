"""Gemini model discovery on the durable serve surface.

CLIProxyAPI opens `GET /v1beta/models` and `GET /v1beta/models/<model>` beside
its Gemini action route (`internal/api/server_routes.go:134`/`:137`, answered by
`sdk/api/handlers/gemini/gemini_handlers.go: GeminiModels` / `GeminiGetHandler`).
A Gemini SDK lists models before it calls one, so without these doors the
`generateContent` route is unreachable for a stock client.

The test also pins the contract that makes discovery useful: the name the
listing hands out (`models/<id>`) is exactly the name the action route accepts.
"""

from __future__ import annotations

import json
import signal
from pathlib import Path
from urllib.parse import urlsplit

import pytest

from tests import process

pytestmark = pytest.mark.integration


def test_serve_lists_and_retrieves_gemini_models(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    import http.client
    import http.server
    import threading

    message = {
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{"type": "text", "text": "kimi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
    }
    requests: list[tuple[str, dict[str, str]]] = []
    owner = type("Fake", (), {})()

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_GET(self) -> None:
            requests.append((self.path, dict(self.headers)))
            self.reply({"data": [{"id": "claude-test", "type": "model"}], "has_more": False})

        def do_POST(self) -> None:
            size = int(self.headers.get("Content-Length", "0"))
            self.rfile.read(size)
            requests.append((self.path, dict(self.headers)))
            self.reply(message)

        def reply(self, payload: dict) -> None:
            body = json.dumps(payload).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    owner.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    owner.thread = threading.Thread(target=owner.server.serve_forever, daemon=True)
    owner.thread.start()
    upstream = f"https://127.0.0.1:{owner.server.server_port}"

    vault = tmp_path / "vault"
    vault.mkdir()
    (vault / "clud_deepseek--api_key_v1.secret").write_text(
        "deepseek-upstream-canary", encoding="utf-8"
    )
    (vault / "clud_kimi--api_key_v1.secret").write_text("kimi-upstream-canary", encoding="utf-8")
    client_token = "local-client-token-0123456789abcdef"
    env = mock_env.copy()
    env.update(
        {
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_STORE_DIR": str(vault),
            "CLUD_SERVE_CLIENT_TOKEN": client_token,
            "CLUD_TEST_GATEWAY_PLAINTEXT_TLS": "1",
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
            # The test-upstream override is all-or-nothing, so every route
            # points at this fake even though only Kimi is exercised.
            "CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL": upstream,
            "CLUD_TEST_UNIFIED_ANTHROPIC_UPSTREAM_URL": upstream,
            "CLUD_TEST_UNIFIED_DEEPSEEK_UPSTREAM_URL": upstream,
            "CLUD_TEST_UNIFIED_OPENROUTER_UPSTREAM_URL": upstream,
            "CLUD_TEST_UNIFIED_KIMI_UPSTREAM_URL": upstream,
        }
    )
    env.pop("ANTHROPIC_API_KEY", None)
    child = process.Popen(
        [str(clud_binary), "serve", "--deepseek", "--kimi", "--port", "0"],
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
        assert address.hostname == "127.0.0.1"

        def call(method: str, path: str, body: dict | None = None):
            connection = http.client.HTTPConnection(address.hostname, address.port, timeout=20)
            headers = {"x-api-key": client_token}
            if body is not None:
                headers["content-type"] = "application/json"
            connection.request(
                method, path, body=None if body is None else json.dumps(body), headers=headers
            )
            response = connection.getresponse()
            data = response.read()
            connection.close()
            return response.status, data

        status, data = call("GET", "/v1beta/models")
        assert status == 200, data
        rows = {row["name"]: row for row in json.loads(data)["models"]}
        assert "models/clud-claude-kimi-k3" in rows, rows
        assert "models/clud-claude-deepseek-flash" in rows, rows
        kimi_row = rows["models/clud-claude-kimi-k3"]
        assert kimi_row["displayName"], kimi_row
        assert "generateContent" in kimi_row["supportedGenerationMethods"], kimi_row
        assert "streamGenerateContent" in kimi_row["supportedGenerationMethods"], kimi_row
        # The catalog carries no reviewed context ceiling for this row, so the
        # field is omitted rather than invented (see `gemini_models` docs).
        assert "inputTokenLimit" not in kimi_row, kimi_row
        # Discovery is a read: it must not spend a provider turn.
        assert not requests

        # The name the listing handed out resolves, with or without the prefix.
        for path in (
            "/v1beta/models/models/clud-claude-kimi-k3",
            "/v1beta/models/clud-claude-kimi-k3",
        ):
            status, data = call("GET", path)
            assert status == 200, (path, data)
            assert json.loads(data)["name"] == "models/clud-claude-kimi-k3", data

        # An unknown name is Gemini's own 404 envelope, not the Anthropic one.
        status, data = call("GET", "/v1beta/models/models/clud-claude-nope")
        assert status == 404, data
        assert json.loads(data)["error"]["status"] == "NOT_FOUND", data

        # A GET with no model name is a malformed request, not a listing.
        status, data = call("GET", "/v1beta/models/")
        assert status == 400, data
        assert json.loads(data)["error"]["status"] == "INVALID_ARGUMENT", data

        # The advertised name is servable: generation accepts it verbatim.
        status, data = call(
            "POST",
            "/v1beta/models/models/clud-claude-kimi-k3:generateContent",
            {"contents": [{"role": "user", "parts": [{"text": "hello"}]}]},
        )
        assert status == 200, data
        payload = json.loads(data)
        assert payload["candidates"][0]["content"]["parts"][0]["text"] == "kimi", payload
        posts = [(path, headers) for path, headers in requests if path.endswith("/v1/messages")]
        assert posts, requests
        # The turn reached the Kimi route, not another provider's.
        assert posts[-1][1].get("Authorization") == "Bearer kimi-upstream-canary", posts[-1]

        child.send_signal(signal.SIGINT)
        assert child.wait(timeout=15) == 0
        stderr = child.stderr.read()
        for secret in (client_token, "kimi-upstream-canary", "deepseek-upstream-canary"):
            assert secret not in stderr
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=15)
        owner.server.shutdown()
        owner.server.server_close()
        owner.thread.join(timeout=5)

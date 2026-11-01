"""Independent API service startup must fail closed without Claude credentials."""

from __future__ import annotations

from pathlib import Path

import pytest

from tests import process

pytestmark = pytest.mark.integration


@pytest.mark.parametrize("extra", [[], ["--openrouter"]])
def test_serve_without_claude_credentials_never_launches_a_harness(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, extra: list[str]
) -> None:
    env = mock_env.copy()
    env["CLUD_INTEGRATION_TESTS"] = "1"
    env["CLUD_TEST_SECRET_STORE_DIR"] = str(tmp_path / "empty-vault")
    env.pop("ANTHROPIC_API_KEY", None)
    env.pop("ANTHROPIC_AUTH_TOKEN", None)
    env.pop("CLAUDE_CODE_OAUTH_TOKEN", None)
    result = process.run(
        [str(clud_binary), "serve", "--claude", *extra],
        env=env,
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    assert result.returncode != 0
    assert "credential" in result.stderr.lower()
    assert "not implemented" not in result.stderr.lower()


def test_serve_missing_openrouter_credential_is_actionable(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    env = mock_env.copy()
    env.update(
        {
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_STORE_DIR": str(tmp_path / "empty-vault"),
            "CLUD_SERVE_CLIENT_TOKEN": "local-client-token-0123456789abcdef",
            "ANTHROPIC_API_KEY": "claude-upstream-canary",
        }
    )
    env.pop("OPENROUTER_API_KEY", None)
    result = process.run(
        [str(clud_binary), "serve", "--claude", "--openrouter"],
        env=env,
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    assert result.returncode != 0
    assert "clud auth login openrouter" in result.stderr
    assert "claude-upstream-canary" not in result.stderr
    assert env["CLUD_SERVE_CLIENT_TOKEN"] not in result.stderr


def test_serve_rejects_reused_client_and_provider_credentials(
    clud_binary: Path, mock_env: dict[str, str]
) -> None:
    env = mock_env.copy()
    secret = "same-client-and-provider-secret-0123456789"
    env["ANTHROPIC_API_KEY"] = secret
    env["CLUD_SERVE_CLIENT_TOKEN"] = secret
    result = process.run(
        [str(clud_binary), "serve", "--claude"],
        env=env,
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    assert result.returncode != 0
    assert "must differ" in result.stderr
    assert secret not in result.stdout + result.stderr


@pytest.fixture
def api_upstreams():
    """Two independent fake providers that record requests and stream Messages."""
    import http.server
    import json
    import threading

    class Provider:
        def __init__(self, marker: str) -> None:
            self.requests: list[tuple[str, dict[str, str], dict]] = []
            self.first_frame = threading.Event()
            self.finish_stream = threading.Event()
            owner = self

            class Handler(http.server.BaseHTTPRequestHandler):
                def log_message(self, format: str, *args: object) -> None:
                    pass

                def do_GET(self) -> None:
                    owner.requests.append((self.path, dict(self.headers), {}))
                    self.reply(
                        {"data": [{"id": "claude-test", "type": "model"}], "has_more": False}
                    )

                def do_POST(self) -> None:
                    size = int(self.headers.get("Content-Length", "0"))
                    body = json.loads(self.rfile.read(size))
                    owner.requests.append((self.path, dict(self.headers), body))
                    if body.get("stream"):
                        payload = (
                            b'event: message_start\ndata: {"type":"message_start",'
                            b'"message":{"id":"msg_test","type":"message","role":"assistant",'
                            b'"model":"claude-test","content":[],"usage":'
                            b'{"input_tokens":1,"output_tokens":0}}}\n\n'
                            b"event: content_block_start\ndata: "
                            b'{"type":"content_block_start","index":0,'
                            b'"content_block":{"type":"tool_use","id":"tool_test",'
                            b'"name":"lookup","input":{}}}\n\n'
                            b'event: message_stop\ndata: {"type":"message_stop"}\n\n'
                        )
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.send_header("Content-Length", str(len(payload)))
                        self.end_headers()
                        first, rest = payload.split(b"\n\n", 1)
                        self.wfile.write(first + b"\n\n")
                        self.wfile.flush()
                        owner.first_frame.set()
                        if owner.finish_stream.wait(timeout=15):
                            self.wfile.write(rest)
                        return
                    self.reply(
                        {
                            "id": "msg_test",
                            "type": "message",
                            "role": "assistant",
                            "model": "claude-test",
                            "content": [{"type": "text", "text": marker}],
                            "stop_reason": "end_turn",
                            "usage": {"input_tokens": 1, "output_tokens": 1},
                        }
                    )

                def reply(self, value: dict) -> None:
                    payload = json.dumps(value).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)

            self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
            self.thread.start()

        @property
        def url(self) -> str:
            return f"http://127.0.0.1:{self.server.server_port}"

        def close(self) -> None:
            self.finish_stream.set()
            self.server.shutdown()
            self.server.server_close()
            self.thread.join(timeout=5)

    claude = Provider("claude")
    openrouter = Provider("openrouter")
    deepseek = Provider("deepseek")
    kimi = Provider("kimi")
    try:
        yield claude, openrouter, deepseek, kimi
    finally:
        claude.close()
        openrouter.close()
        deepseek.close()
        kimi.close()


def test_serve_http_routes_streams_and_isolates_credentials(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, api_upstreams
) -> None:
    import http.client
    import json
    import signal
    from urllib.parse import urlsplit

    claude, openrouter, _, _ = api_upstreams
    vault = tmp_path / "vault"
    vault.mkdir()
    (vault / "clud_openrouter--api_key_v1.secret").write_text(
        "openrouter-upstream-canary", encoding="utf-8"
    )
    client_token = "local-client-token-0123456789abcdef"
    env = mock_env.copy()
    env.update(
        {
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_STORE_DIR": str(vault),
            "CLUD_SERVE_CLIENT_TOKEN": client_token,
            "ANTHROPIC_API_KEY": "claude-upstream-canary",
            "CLUD_TEST_GATEWAY_PLAINTEXT_TLS": "1",
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
            "CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL": claude.url.replace("http://", "https://"),
            "CLUD_TEST_UNIFIED_ANTHROPIC_UPSTREAM_URL": claude.url.replace("http://", "https://"),
            "CLUD_TEST_UNIFIED_DEEPSEEK_UPSTREAM_URL": openrouter.url.replace(
                "http://", "https://"
            ),
            "CLUD_TEST_UNIFIED_OPENROUTER_UPSTREAM_URL": openrouter.url.replace(
                "http://", "https://"
            ),
        }
    )
    child = process.Popen(
        [str(clud_binary), "serve", "--claude", "--openrouter", "--port", "0"],
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
        for secret in (client_token, "claude-upstream-canary", "openrouter-upstream-canary"):
            assert secret not in line
        address = urlsplit(line.strip().split("listening at ", 1)[1])
        assert address.hostname == "127.0.0.1"

        def call(method: str, path: str, body: dict | None, token: str = client_token):
            connection = http.client.HTTPConnection(address.hostname, address.port, timeout=15)
            headers = {"x-api-key": token, "anthropic-version": "2023-06-01"}
            if body is not None:
                headers["content-type"] = "application/json"
            connection.request(
                method, path, body=None if body is None else json.dumps(body), headers=headers
            )
            response = connection.getresponse()
            data = response.read()
            connection.close()
            return response.status, data

        assert call("GET", "/v1/models", None, "invalid")[0] == 401
        assert not claude.requests
        assert not openrouter.requests
        malformed = http.client.HTTPConnection(address.hostname, address.port, timeout=15)
        malformed.request("POST", "/v1/messages", body="{", headers={"x-api-key": client_token})
        bad_response = malformed.getresponse()
        assert bad_response.status == 400
        bad_response.read()
        malformed.close()
        assert not claude.requests
        assert not openrouter.requests
        status, models = call("GET", "/v1/models", None)
        assert status == 200
        ids = {row["id"] for row in json.loads(models)["data"]}
        assert {"claude-test", "clud-claude-openrouter-sonnet"} <= ids
        request_body = {
            "model": "claude-test",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 64,
        }
        status, body = call("POST", "/v1/messages", request_body)
        assert status == 200
        assert json.loads(body)["content"][0]["text"] == "claude"
        stream = http.client.HTTPConnection(address.hostname, address.port, timeout=15)
        stream.request(
            "POST",
            "/v1/messages",
            body=json.dumps(
                {
                    **request_body,
                    "stream": True,
                    "tools": [{"name": "lookup", "input_schema": {"type": "object"}}],
                }
            ),
            headers={"x-api-key": client_token, "content-type": "application/json"},
        )
        streaming = stream.getresponse()
        assert streaming.status == 200
        assert streaming.readline() == b"event: message_start\n"
        assert claude.first_frame.wait(timeout=5)
        assert not claude.finish_stream.is_set()
        # A headerless client can complete a second request while this stream waits.
        parallel = http.client.HTTPConnection(address.hostname, address.port, timeout=5)
        parallel.request(
            "POST",
            "/v1/messages",
            body=json.dumps(request_body),
            headers={"x-api-key": client_token, "content-type": "application/json"},
        )
        parallel_response = parallel.getresponse()
        assert parallel_response.status == 200
        assert json.loads(parallel_response.read())["content"][0]["text"] == "claude"
        parallel.close()
        assert not claude.finish_stream.is_set()
        claude.finish_stream.set()
        body = streaming.read()
        stream.close()
        assert b'"tool_use"' in body
        assert b"message_stop" in body
        status, body = call(
            "POST", "/v1/messages", {**request_body, "model": "clud-claude-openrouter-sonnet"}
        )
        assert status == 200
        assert json.loads(body)["content"][0]["text"] == "openrouter"
        assert len(openrouter.requests) == 1
        assert openrouter.requests[0][2]["model"] != "clud-claude-openrouter-sonnet"
        for _, headers, _ in claude.requests:
            assert "claude-upstream-canary" in headers.values()
            assert client_token not in headers.values()
            assert "openrouter-upstream-canary" not in headers.values()
        for _, headers, _ in openrouter.requests:
            assert any("openrouter-upstream-canary" in value for value in headers.values())
            assert client_token not in headers.values()
            assert "claude-upstream-canary" not in headers.values()
        # Shutdown must not wait for a stalled upstream stream.
        claude.first_frame.clear()
        claude.finish_stream.clear()
        active = http.client.HTTPConnection(address.hostname, address.port, timeout=15)
        active.request(
            "POST",
            "/v1/messages",
            body=json.dumps({**request_body, "stream": True}),
            headers={"x-api-key": client_token, "content-type": "application/json"},
        )
        active_response = active.getresponse()
        assert active_response.readline() == b"event: message_start\n"
        assert claude.first_frame.wait(timeout=5)
        child.send_signal(signal.SIGINT)
        assert child.wait(timeout=5) == 0
        active.close()
        stderr = child.stderr.read()
        for secret in (client_token, "claude-upstream-canary", "openrouter-upstream-canary"):
            assert secret not in stderr
    finally:
        claude.finish_stream.set()
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=15)


def test_serve_answers_openai_chat_completions(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, api_upstreams
) -> None:
    """The durable serve surface accepts and answers the OpenAI chat shape.

    The served endpoint reuses the gateway's route table, so a chat caller must
    work there too: translated in at the door and back out at the response.
    """
    import http.client
    import json
    import signal
    from urllib.parse import urlsplit

    claude, openrouter, _, _ = api_upstreams
    client_token = "local-client-token-0123456789abcdef"
    vault = tmp_path / "vault"
    vault.mkdir()
    env = mock_env.copy()
    env.update(
        {
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_STORE_DIR": str(vault),
            "CLUD_SERVE_CLIENT_TOKEN": client_token,
            "ANTHROPIC_API_KEY": "claude-upstream-canary",
            "CLUD_TEST_GATEWAY_PLAINTEXT_TLS": "1",
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
            "CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL": claude.url.replace("http://", "https://"),
            "CLUD_TEST_UNIFIED_ANTHROPIC_UPSTREAM_URL": claude.url.replace(
                "http://", "https://"
            ),
            # The test-upstream override is all-or-nothing, so every route needs
            # a fake even though this test only exercises Claude.
            "CLUD_TEST_UNIFIED_DEEPSEEK_UPSTREAM_URL": openrouter.url.replace(
                "http://", "https://"
            ),
            "CLUD_TEST_UNIFIED_OPENROUTER_UPSTREAM_URL": openrouter.url.replace(
                "http://", "https://"
            ),
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
        assert client_token not in line
        assert "claude-upstream-canary" not in line
        address = urlsplit(line.strip().split("listening at ", 1)[1])
        assert address.hostname is not None
        assert child.stderr is not None

        connection = http.client.HTTPConnection(address.hostname, address.port, timeout=15)
        request_body = {
            "model": "claude-test",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": False,
        }
        connection.request(
            "POST",
            "/v1/chat/completions",
            body=json.dumps(request_body),
            headers={"x-api-key": client_token, "content-type": "application/json"},
        )
        response = connection.getresponse()
        data = response.read()
        connection.close()
        assert response.status == 200, data
        payload = json.loads(data)
        assert payload["object"] == "chat.completion", payload
        assert payload["choices"][0]["message"]["content"] == "claude", payload
        assert payload["choices"][0]["finish_reason"] == "stop", payload
        # What reached the backend was a Messages request, not the chat shape.
        _, _, upstream_body = claude.requests[-1]
        assert upstream_body["messages"][0]["role"] == "user", upstream_body
        assert "max_completion_tokens" not in upstream_body, upstream_body

        child.send_signal(signal.SIGINT)
        assert child.wait(timeout=15) == 0
        stderr = child.stderr.read()
        assert client_token not in stderr
        assert "claude-upstream-canary" not in stderr
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=15)


@pytest.mark.parametrize("streaming", [False, True])
def test_serve_codex_without_claude_key(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, streaming: bool
) -> None:
    import http.client
    import json
    import signal
    from urllib.parse import urlsplit

    from tests.integration.test_mock_agents import _FakeResponsesServer

    upstream = _FakeResponsesServer()
    token = "codex-client-token-0123456789abcdef"
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
        assert token not in line
        address = urlsplit(line.strip().split("listening at ", 1)[1])

        def call(method, path, payload=None, client_token=token):
            connection = http.client.HTTPConnection(address.hostname, address.port, timeout=15)
            connection.request(
                method,
                path,
                body=json.dumps(payload) if payload is not None else None,
                headers={"x-api-key": client_token, "content-type": "application/json"},
            )
            response = connection.getresponse()
            result = response.status, response.read()
            connection.close()
            return result

        assert call("GET", "/v1/models", client_token="wrong")[0] == 401
        status, body = call("GET", "/v1/models")
        assert status == 200, body
        ids = {row["id"] for row in json.loads(body)["data"]}
        assert "clud-claude-codex-sol" in ids
        assert all(model.startswith("clud-claude-codex-") for model in ids)
        assert not upstream.requests
        payload = {
            "model": "clud-claude-codex-sol",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hello"}],
            "stream": streaming,
        }
        assert call("POST", "/v1/messages", payload, "wrong")[0] == 401
        assert not upstream.requests
        status, body = call("POST", "/v1/messages", payload)
        assert status == 200, body
        if streaming:
            assert b"event: message_stop" in body
            assert b"bridged " in body
        else:
            assert "bridged reply" in json.loads(body)["content"][0]["text"]
        assert len(upstream.requests) == 1
        if not streaming:
            for _ in range(40):
                status, body = call("POST", "/v1/messages", payload)
                assert status == 200, body
        headers, _, raw = upstream.requests[0].partition(b"\r\n\r\n")
        assert token.encode() not in headers
        translated = json.loads(raw)
        assert "input" in translated
        assert translated["model"] != payload["model"]
        child.send_signal(signal.SIGINT)
        assert child.wait(timeout=15) == 0
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=15)
        upstream.close()


def test_serve_codex_missing_credentials_is_actionable(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    env = mock_env.copy()
    env.update({"HOME": str(tmp_path), "USERPROFILE": str(tmp_path)})
    env.pop("OPENAI_API_KEY", None)
    result = process.run(
        [str(clud_binary), "serve", "--codex"],
        env=env,
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    assert result.returncode == 2
    assert "clud auth login codex" in result.stderr
    assert "ANTHROPIC_API_KEY" not in result.stderr


def test_serve_anthropic_compat_providers_route_by_model(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, api_upstreams
) -> None:
    """`--deepseek` and `--kimi` put the registry's other direct routes on the
    served endpoint, each reached with its own vault key.

    CLIProxyAPI's server answers every configured upstream from one listener;
    clud's serve surface previously offered Claude, Codex and OpenRouter only,
    so DeepSeek and Kimi keys were usable in a launch but not from the durable
    endpoint.
    """
    import http.client
    import json
    import signal
    from urllib.parse import urlsplit

    claude, openrouter, deepseek, kimi = api_upstreams
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
            "CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL": claude.url.replace("http://", "https://"),
            "CLUD_TEST_UNIFIED_ANTHROPIC_UPSTREAM_URL": claude.url.replace("http://", "https://"),
            "CLUD_TEST_UNIFIED_DEEPSEEK_UPSTREAM_URL": deepseek.url.replace(
                "http://", "https://"
            ),
            "CLUD_TEST_UNIFIED_OPENROUTER_UPSTREAM_URL": openrouter.url.replace(
                "http://", "https://"
            ),
            "CLUD_TEST_UNIFIED_KIMI_UPSTREAM_URL": kimi.url.replace("http://", "https://"),
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
        for secret in (client_token, "deepseek-upstream-canary", "kimi-upstream-canary"):
            assert secret not in line
        address = urlsplit(line.strip().split("listening at ", 1)[1])
        assert address.hostname == "127.0.0.1"

        def call(method: str, path: str, body: dict | None):
            connection = http.client.HTTPConnection(address.hostname, address.port, timeout=15)
            headers = {"x-api-key": client_token, "anthropic-version": "2023-06-01"}
            if body is not None:
                headers["content-type"] = "application/json"
            connection.request(
                method, path, body=None if body is None else json.dumps(body), headers=headers
            )
            response = connection.getresponse()
            data = response.read()
            connection.close()
            return response.status, data

        status, models = call("GET", "/v1/models", None)
        assert status == 200
        ids = {row["id"] for row in json.loads(models)["data"]}
        assert {"clud-claude-kimi-k3", "clud-claude-deepseek-flash"} <= ids
        assert not claude.requests

        def turn(model: str) -> str:
            status, body = call(
                "POST",
                "/v1/messages",
                {
                    "model": model,
                    "messages": [{"role": "user", "content": "hello"}],
                    "max_tokens": 64,
                },
            )
            assert status == 200, body
            return json.loads(body)["content"][0]["text"]

        assert turn("clud-claude-kimi-k3") == "kimi"
        assert turn("clud-claude-deepseek-flash") == "deepseek"
        kimi_paths = [path for path, _, _ in kimi.requests]
        deepseek_paths = [path for path, _, _ in deepseek.requests]
        assert kimi_paths
        assert deepseek_paths
        assert all(path.endswith("/v1/messages") for path in kimi_paths)
        assert all(path.endswith("/v1/messages") for path in deepseek_paths)
        # Each upstream saw only its own key: the routes never share a secret.
        assert all(
            header.get("Authorization") == "Bearer kimi-upstream-canary"
            for _, header, _ in kimi.requests
        )
        assert all(
            header.get("Authorization") == "Bearer deepseek-upstream-canary"
            for _, header, _ in deepseek.requests
        )
        assert not openrouter.requests
    finally:
        child.send_signal(signal.SIGINT)
        child.wait(timeout=15)

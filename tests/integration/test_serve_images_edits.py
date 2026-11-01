"""`POST /v1/images/edits` on the durable `clud serve` endpoint.

CLIProxyAPI routes OpenAI's edits door through the same Codex executor and
rewrites the multipart form into the JSON contract
(``internal/runtime/executor/codex_openai_images.go``,
``codexRewriteOpenAIImageEditMultipartToJSON``). clud accepted only JSON
generations before this door. These subprocess tests prove the rewrite reaches
a real fake upstream and that a refused form costs no upstream call.
"""

from __future__ import annotations

import json
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from tests import process

pytestmark = pytest.mark.integration

_IMAGE_PAYLOAD = {
    "created": int(1e9),
    "data": [{"b64_json": "ZWRpdGVk", "revised_prompt": "a red cube, blue"}],
}

_PNG = b"\x89PNG\r\n\x1a\n" + b"0" * 8


class _FakeImagesServer:
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


def _multipart(fields: list[tuple[str, str]], files: list[tuple[str, bytes]]) -> tuple[bytes, str]:
    boundary = "----clud" + uuid.uuid4().hex
    chunks: list[bytes] = []
    for name, value in fields:
        chunks.append(f"--{boundary}\r\n".encode())
        chunks.append(f'Content-Disposition: form-data; name="{name}"\r\n\r\n'.encode())
        chunks.append(value.encode())
        chunks.append(b"\r\n")
    for name, data in files:
        chunks.append(f"--{boundary}\r\n".encode())
        chunks.append(
            f'Content-Disposition: form-data; name="{name}"; filename="{name}.png"\r\n'.encode()
        )
        chunks.append(b"Content-Type: image/png\r\n\r\n")
        chunks.append(data)
        chunks.append(b"\r\n")
    chunks.append(f"--{boundary}--\r\n".encode())
    return b"".join(chunks), f"multipart/form-data; boundary={boundary}"


def _post(address, path: str, body: bytes, content_type: str, token: str):
    import http.client

    connection = http.client.HTTPConnection(address.hostname, address.port, timeout=30)
    connection.request(
        "POST",
        path,
        body=body,
        headers={"x-api-key": token, "content-type": content_type},
    )
    response = connection.getresponse()
    result = response.status, response.read()
    connection.close()
    return result


def _serve(clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, upstream, token: str):
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
    return process.Popen(
        [str(clud_binary), "serve", "--codex", "--port", "0"],
        env=env,
        stdout=process.PIPE,
        stderr=process.PIPE,
        text=True,
        creationflags=process.CREATE_NEW_PROCESS_GROUP,
    )


def test_serve_rewrites_a_multipart_image_edit_into_json(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    from urllib.parse import urlsplit

    upstream = _FakeImagesServer()
    token = "images-client-token-0123456789abcd"
    child = _serve(clud_binary, mock_env, tmp_path, upstream, token)
    try:
        line = child.next_stdout(timeout=60)
        assert isinstance(line, str), (line, child.poll())
        address = urlsplit(line.strip().split("listening at ", 1)[1])

        body, content_type = _multipart(
            [("model", "gpt-image-1.5"), ("prompt", "make it blue"), ("n", "1")],
            [("image", _PNG), ("mask", b"mask-bytes")],
        )
        status, answer = _post(address, "/v1/images/edits", body, content_type, token)
        assert status == 200, answer
        assert json.loads(answer) == _IMAGE_PAYLOAD
        assert len(upstream.requests) == 1
        path, headers, raw = upstream.requests[0]
        assert path.endswith("/images/generations"), path
        forwarded = json.loads(raw)
        assert forwarded["model"] == "gpt-image-1.5"
        assert forwarded["prompt"] == "make it blue"
        assert forwarded["n"] == 1
        assert forwarded["images"][0]["image_url"].startswith("data:image/png;base64,")
        assert forwarded["mask"]["image_url"].startswith("data:image/png;base64,")
        # The client token never becomes the upstream bearer.
        assert token not in headers.get("Authorization", "")
        assert token.encode() not in raw
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=15)
        upstream.close()


def test_serve_refuses_an_edit_without_an_image_before_spending(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    from urllib.parse import urlsplit

    upstream = _FakeImagesServer()
    token = "images-client-token-0123456789abcd"
    child = _serve(clud_binary, mock_env, tmp_path, upstream, token)
    try:
        line = child.next_stdout(timeout=60)
        assert isinstance(line, str), (line, child.poll())
        address = urlsplit(line.strip().split("listening at ", 1)[1])

        body, content_type = _multipart(
            [("model", "gpt-image-1.5"), ("prompt", "make it blue")], []
        )
        status, answer = _post(address, "/v1/images/edits", body, content_type, token)
        assert status == 400, answer
        envelope = json.loads(answer)
        assert envelope["error"]["type"] == "invalid_request_error"
        assert "image" in envelope["error"]["message"]
        # A JSON body on the edits door keeps working and is still validated.
        status, answer = _post(
            address,
            "/v1/images/edits",
            json.dumps({"model": "dall-e-3", "prompt": "x"}).encode(),
            "application/json",
            token,
        )
        assert status == 400, answer
        assert "dall-e-3" in json.loads(answer)["error"]["message"]
        assert not upstream.requests
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=15)
        upstream.close()

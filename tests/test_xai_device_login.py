"""End-to-end coverage for the xAI device-authorization login seam.

Ports CLIProxyAPI ``internal/auth/xai/{xai,types,token}.go`` (RFC 8628 device
grant). The Rust unit tests drive the state machine directly; these tests run
the real binary through its hidden ``__xai-device-login`` seam against a local
OAuth server, so argument parsing, discovery, polling and the JSON progress
contract are exercised across the process boundary.
"""

from __future__ import annotations

import base64
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

from tests import process


def _binary_name(name: str) -> str:
    return f"{name}.exe" if sys.platform == "win32" else name


def _clud_binary() -> Path:
    configured = os.environ.get("CLUD_TEST_BINARY")
    if configured:
        return Path(configured)
    candidate = Path(__file__).resolve().parents[1] / "target" / "debug" / _binary_name("clud")
    assert candidate.is_file(), f"clud test binary not found at {candidate}"
    return candidate


def _base64url(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode("ascii")


def _id_token(email: str, subject: str) -> str:
    header = _base64url(json.dumps({"alg": "none"}).encode())
    claims = _base64url(json.dumps({"email": email, "sub": subject}).encode())
    return f"{header}.{claims}.signature"


class _OAuthServer:
    """A scripted xAI-shaped OAuth server on loopback."""

    def __init__(self, token_responses: list[tuple[int, dict[str, object]]]) -> None:
        self.token_responses = list(token_responses)
        self.requests: list[tuple[str, str]] = []
        self._server = HTTPServer(("127.0.0.1", 0), self._handler())
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    @property
    def base_url(self) -> str:
        host, port = self._server.server_address[:2]
        return f"http://{host}:{port}"

    def _handler(self) -> type[BaseHTTPRequestHandler]:
        outer = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, format: str, *args: object) -> None:
                return

            def do_GET(self) -> None:
                body = json.dumps(
                    {
                        "device_authorization_endpoint": f"{outer.base_url}/device",
                        "token_endpoint": f"{outer.base_url}/token",
                    }
                ).encode()
                outer.requests.append(("GET", self.path))
                self._respond(200, body)

            def do_POST(self) -> None:
                length = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(length).decode()
                outer.requests.append((self.path, body))
                if self.path == "/device":
                    self._respond(
                        200,
                        json.dumps(
                            {
                                "device_code": "device-abc",
                                "user_code": "USER-123",
                                "verification_uri": "https://x.ai/device",
                                "expires_in": 60,
                                "interval": 1,
                            }
                        ).encode(),
                    )
                    return
                if not outer.token_responses:
                    self._respond(400, json.dumps({"error": "test_exhausted"}).encode())
                    return
                status, payload = outer.token_responses.pop(0)
                self._respond(status, json.dumps(payload).encode())

            def _respond(self, status: int, body: bytes) -> None:
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(body)

        return Handler

    def __enter__(self) -> _OAuthServer:
        self._thread.start()
        return self

    def __exit__(self, *_exc: object) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)


def _run(args: list[str]) -> process.CompletedProcess[object]:
    return process.run(
        [str(_clud_binary()), "__xai-device-login", *args],
        capture_output=True,
        text=True,
        timeout=60,
    )


def test_device_login_reports_the_code_then_authorizes() -> None:
    identity = _id_token("zach@example.com", "sub-42")
    with _OAuthServer(
        [
            (400, {"error": "authorization_pending"}),
            (
                200,
                {
                    "access_token": "access-1",
                    "refresh_token": "refresh-1",
                    "id_token": identity,
                    "token_type": "Bearer",
                    "expires_in": 3600,
                },
            ),
        ]
    ) as server:
        completed = _run(
            [
                "--discovery-url",
                f"{server.base_url}/discovery",
                "--interval-ms",
                "5",
                "--max-poll-ms",
                "5000",
            ]
        )
        assert completed.returncode == 0, completed.stderr
        events = [json.loads(line) for line in completed.stdout.splitlines() if line.strip()]
        assert [event["event"] for event in events] == ["device_code", "authorized"]
        assert events[0]["user_code"] == "USER-123"
        assert events[0]["verification_uri"] == "https://x.ai/device"
        assert events[1]["email"] == "zach@example.com"
        assert events[1]["subject"] == "sub-42"
        assert events[1]["has_refresh_token"] is True
        # The access token must never be printed by the seam.
        assert "access-1" not in completed.stdout
        token_polls = [body for path, body in server.requests if path == "/token"]
        assert len(token_polls) == 2
        device_grant = "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        assert all(device_grant in body for body in token_polls)


def test_denied_authorization_exits_nonzero_with_the_reason() -> None:
    with _OAuthServer([(400, {"error": "access_denied"})]) as server:
        completed = _run(
            [
                "--discovery-url",
                f"{server.base_url}/discovery",
                "--interval-ms",
                "5",
                "--max-poll-ms",
                "5000",
            ]
        )
        assert completed.returncode == 1
        assert "xai device authorization denied" in completed.stderr


def test_explicit_endpoints_skip_discovery_but_must_stay_on_xai() -> None:
    with _OAuthServer([]) as server:
        rejected = _run(
            [
                "--device-endpoint",
                "https://evil.example/device",
                "--token-endpoint",
                "https://evil.example/token",
            ]
        )
        assert rejected.returncode == 1
        assert "is not on x.ai" in rejected.stderr
        assert server.requests == []

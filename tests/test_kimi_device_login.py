"""End-to-end coverage for the Kimi Code device-authorization login seam.

Ports CLIProxyAPI ``internal/auth/kimi/{kimi,token}.go`` (RFC 8628 device
grant for a Kimi Code plan). The Rust unit tests drive the state machine and
the refresh exchange directly; these tests run the real binary through its
hidden ``__kimi-device-login`` seam against a local OAuth server, so argument
parsing, the ``X-Msh-*`` device headers, polling, and the JSON progress
contract are exercised across the process boundary.

Kimi answers **200** with an ``error`` field while the user has not yet
authorized, which is the one shape that differs from xAI; the server below
reproduces it.
"""

from __future__ import annotations

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


class _KimiOAuthServer:
    """A scripted Kimi-shaped OAuth server on loopback."""

    def __init__(self, token_responses: list[tuple[int, dict[str, object]]]) -> None:
        self.token_responses = list(token_responses)
        self.requests: list[tuple[str, str, dict[str, str]]] = []
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

            def do_POST(self) -> None:
                length = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(length).decode()
                headers = {name.lower(): value for name, value in self.headers.items()}
                outer.requests.append((self.path, body, headers))
                if self.path == "/device":
                    self._respond(
                        200,
                        json.dumps(
                            {
                                "device_code": "device-abc",
                                "user_code": "USER-123",
                                "verification_uri": "https://kimi.com/device",
                                "verification_uri_complete": "https://kimi.com/device?code=USER-123",
                                "expires_in": 60,
                                "interval": 1,
                            }
                        ).encode(),
                    )
                    return
                if not outer.token_responses:
                    self._respond(200, json.dumps({"error": "test_exhausted"}).encode())
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

    def __enter__(self) -> _KimiOAuthServer:
        self._thread.start()
        return self

    def __exit__(self, *_exc: object) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)


def _run(args: list[str]) -> process.CompletedProcess[object]:
    return process.run(
        [str(_clud_binary()), "__kimi-device-login", *args],
        capture_output=True,
        text=True,
        timeout=60,
    )


def _loopback_args(server: _KimiOAuthServer, *, domain: str | None = None) -> list[str]:
    args = [
        "--device-endpoint",
        f"{server.base_url}/device",
        "--token-endpoint",
        f"{server.base_url}/token",
        "--interval-ms",
        "5",
        "--max-poll-ms",
        "5000",
    ]
    if domain is not None:
        args.extend(["--domain", domain])
    return args


def test_device_login_reports_the_code_then_authorizes() -> None:
    with _KimiOAuthServer(
        [
            (200, {"error": "authorization_pending"}),
            (
                200,
                {
                    "access_token": "access-1",
                    "refresh_token": "refresh-1",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "scope": "coding",
                },
            ),
        ]
    ) as server:
        completed = _run(_loopback_args(server))
        assert completed.returncode == 0, completed.stderr
        events = [json.loads(line) for line in completed.stdout.splitlines() if line.strip()]
        assert [event["event"] for event in events] == ["device_code", "authorized"]
        assert events[0]["user_code"] == "USER-123"
        assert events[0]["verification_uri"] == "https://kimi.com/device"
        assert events[0]["domain"] == "kimi.com"
        assert events[1]["details"]["domain"] == "kimi.com"
        assert events[1]["details"]["api_base_url"] == "https://api.kimi.com/coding"
        assert events[1]["details"]["has_refresh_token"] is True
        assert events[1]["details"]["expires_in"] == 3600
        # The seam must never print a token.
        assert "access-1" not in completed.stdout
        assert "refresh-1" not in completed.stdout
        token_polls = [
            (body, headers) for path, body, headers in server.requests if path == "/token"
        ]
        assert len(token_polls) == 2
        device_grant = "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        assert all(device_grant in body for body, _headers in token_polls)
        # Every request carries Kimi's device identity headers.
        for _path, _body, headers in server.requests:
            assert headers["x-msh-device-id"]
            assert headers["x-msh-platform"] == "CLUD"
            assert headers["x-msh-version"]
            assert headers["x-msh-device-model"]


def test_domain_ai_selects_the_kimi_ai_api_base() -> None:
    with _KimiOAuthServer([(200, {"access_token": "access-1", "expires_in": 60})]) as server:
        completed = _run(_loopback_args(server, domain="ai"))
        assert completed.returncode == 0, completed.stderr
        events = [json.loads(line) for line in completed.stdout.splitlines() if line.strip()]
        assert events[0]["domain"] == "kimi.ai"
        assert events[1]["details"]["api_base_url"] == "https://api.kimi.ai/coding"


def test_denied_authorization_exits_nonzero_with_the_reason() -> None:
    with _KimiOAuthServer([(200, {"error": "access_denied"})]) as server:
        completed = _run(_loopback_args(server))
        assert completed.returncode == 1
        assert "kimi device authorization denied" in completed.stderr


def test_expired_code_exits_nonzero() -> None:
    with _KimiOAuthServer([(200, {"error": "expired_token"})]) as server:
        completed = _run(_loopback_args(server))
        assert completed.returncode == 1
        assert "kimi device code expired" in completed.stderr


def test_explicit_endpoints_must_stay_on_kimi_or_loopback() -> None:
    rejected = _run(
        [
            "--device-endpoint",
            "https://evil.example/device",
            "--token-endpoint",
            "https://evil.example/token",
        ]
    )
    assert rejected.returncode == 1
    assert "must be an https kimi.com/kimi.ai endpoint" in rejected.stderr


def test_one_endpoint_without_the_other_is_refused() -> None:
    # clap rejects the pair before the seam runs; the seam keeps its own
    # "both are required" guard for callers that are not the CLI.
    completed = _run(["--device-endpoint", "https://auth.kimi.com/api/oauth/device_authorization"])
    assert completed.returncode != 0
    output = completed.stderr + completed.stdout
    assert "--token-endpoint" in output
    assert "required" in output

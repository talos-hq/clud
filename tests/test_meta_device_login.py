"""End-to-end coverage for the Meta Muse device-authorization login seam.

Ports CLIProxyAPI ``internal/auth/meta/meta.go`` (RFC 8628 device grant against
``auth.meta.com`` plus the ``api.meta.ai/muse-code/key`` API-key mint). The Rust
unit tests drive the state machine, the mint exchange and the credential record
directly; these tests run the real binary through its hidden
``__meta-device-login`` seam against a local OAuth + mint server, so argument
parsing, the poll contract, the mint bearer, vault persistence and the JSON
progress contract are exercised across the process boundary.

The vault is redirected to a temp dir with ``CLUD_TEST_SECRET_STORE_DIR`` so a
test login can never write into the developer's real vault.
"""

from __future__ import annotations

import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from typing import Any

from tests import process


def _binary_name(name: str) -> str:
    return f"{name}.exe" if sys.platform == "win32" else name


def _clud_binary() -> Path:
    configured = os.environ.get("CLUD_TEST_BINARY")
    if configured:
        return Path(configured)
    candidate = (
        Path(__file__).resolve().parents[1] / "target" / "debug" / _binary_name("clud")
    )
    assert candidate.is_file(), f"clud test binary not found at {candidate}"
    return candidate


class _MetaServer:
    """A scripted Meta-shaped OAuth + mint server on loopback."""

    def __init__(
        self,
        token_responses: list[tuple[int, dict[str, object]]],
        mint_response: tuple[int, dict[str, object]] | None = None,
    ) -> None:
        self.token_responses = list(token_responses)
        self.mint_response = mint_response or (
            200,
            {"api_key": "mk-1", "base_url": "https://muse.example/v1"},
        )
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
                        {
                            "device_code": "device-abc",
                            "user_code": "USER-123",
                            "verification_uri": "https://meta.com/device",
                            "verification_uri_complete": "https://meta.com/device?code=USER-123",
                            "expires_in": 60,
                            "interval": 1,
                        },
                    )
                    return
                if self.path == "/mint":
                    status, payload = outer.mint_response
                    self._respond(status, payload)
                    return
                if not outer.token_responses:
                    self._respond(200, {"error": "test_exhausted"})
                    return
                status, payload = outer.token_responses.pop(0)
                self._respond(status, payload)

            def _respond(self, status: int, payload: dict[str, object]) -> None:
                body = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(body)

        return Handler

    def paths(self) -> list[str]:
        return [path for path, _body, _headers in self.requests]

    def __enter__(self) -> _MetaServer:
        self._thread.start()
        return self

    def __exit__(self, *_exc: object) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)


def _env(vault: Path) -> dict[str, str]:
    env = dict(os.environ)
    env["CLUD_INTEGRATION_TESTS"] = "1"
    env["CLUD_TEST_SECRET_STORE_DIR"] = str(vault)
    return env


def _run(vault: Path, args: list[str]) -> process.CompletedProcess[object]:
    return process.run(
        [str(_clud_binary()), "__meta-device-login", *args],
        env=_env(vault),
        capture_output=True,
        text=True,
        timeout=60,
    )


def _loopback_args(server: _MetaServer) -> list[str]:
    return [
        "--device-endpoint",
        f"{server.base_url}/device",
        "--token-endpoint",
        f"{server.base_url}/token",
        "--mint-url",
        f"{server.base_url}/mint",
        "--interval-ms",
        "5",
        "--max-poll-ms",
        "5000",
    ]


def _events(completed: process.CompletedProcess[object]) -> list[dict[str, Any]]:
    return [json.loads(line) for line in (completed.stdout or "").splitlines() if line.strip()]


def test_device_login_reports_the_code_then_mints_and_stores(tmp_path: Path) -> None:
    with _MetaServer(
        [
            (400, {"error": "authorization_pending"}),
            (400, {"error": "slow_down"}),
            (
                200,
                {"access_token": "dca-1", "token_type": "Bearer", "expires_in": 600},
            ),
        ],
        mint_response=(
            200,
            {
                "api_key": "mk-1",
                "base_url": "https://muse.example/v1",
                "user_email": "zach@example.com",
                "user_full_name": "Zach",
                "subs_tier_name": "Muse Code",
                "is_subs_active": True,
            },
        ),
    ) as server:
        completed = _run(tmp_path, _loopback_args(server))
        assert completed.returncode == 0, completed.stderr
        events = _events(completed)
        assert [event["event"] for event in events] == ["device_code", "authorized"]
        assert events[0]["user_code"] == "USER-123"
        assert events[0]["verification_uri"] == "https://meta.com/device"
        details = events[1]["details"]
        assert details["has_api_key"] is True
        assert details["account"]["email"] == "zach@example.com"
        assert details["account"]["base_url"] == "https://muse.example/v1"
        assert details["account"]["subs_active"] is True
        # The seam must never print a credential.
        assert "mk-1" not in (completed.stdout or "")
        assert "dca-1" not in (completed.stdout or "")
        # The mint call carries the dca token as a bearer and in the body.
        mint_calls = [
            (body, headers) for path, body, headers in server.requests if path == "/mint"
        ]
        assert len(mint_calls) == 1
        assert mint_calls[0][1]["authorization"] == "Bearer dca-1"
        assert "dca-1" in mint_calls[0][0]
        # Polls carry the device-code grant and the client id.
        token_calls = [body for path, body, _headers in server.requests if path == "/token"]
        assert len(token_calls) == 3
        for body in token_calls:
            assert "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code" in body
            assert "device_code=device-abc" in body
            assert "client_id=1031625952748946" in body
        # Every request identifies as the Muse Code CLI.
        assert all(
            headers["user-agent"] == "muse-code/1.0.2"
            for _path, _body, headers in server.requests
        )
        # The credential landed in the redirected vault.
        stored = list(tmp_path.glob("*.secret"))
        assert len(stored) == 1
        assert "mk-1" in stored[0].read_text()


def test_status_reports_the_stored_account_without_network(tmp_path: Path) -> None:
    with _MetaServer(
        [(200, {"access_token": "dca-1", "token_type": "Bearer", "expires_in": 600})],
        mint_response=(
            200,
            {
                "api_key": "mk-1",
                "base_url": "https://muse.example/v1",
                "user_email": "zach@example.com",
            },
        ),
    ) as server:
        first = _run(tmp_path, _loopback_args(server))
        assert first.returncode == 0, first.stderr
        requests_before = len(server.requests)
        completed = _run(tmp_path, ["--status"])
        assert completed.returncode == 0, completed.stderr
        assert len(server.requests) == requests_before
        events = _events(completed)
        assert [event["event"] for event in events] == ["status"]
        details = events[0]["details"]
        assert details["has_api_key"] is True
        assert details["email"] == "zach@example.com"
        assert details["base_url"] == "https://muse.example/v1"
        assert "mk-1" not in (completed.stdout or "")


def test_a_missing_stored_login_reports_null_status(tmp_path: Path) -> None:
    completed = _run(tmp_path, ["--status"])
    assert completed.returncode == 0, completed.stderr
    events = _events(completed)
    assert events == [{"event": "status", "details": None}]


def test_a_failed_mint_keeps_the_dca_login_and_says_so(tmp_path: Path) -> None:
    with _MetaServer(
        [(200, {"access_token": "dca-1", "token_type": "Bearer", "expires_in": 600})],
        mint_response=(500, {"error": "denied", "error_description": "no tier"}),
    ) as server:
        completed = _run(tmp_path, _loopback_args(server))
        assert completed.returncode == 0, completed.stderr
        events = _events(completed)
        details = events[-1]["details"]
        assert details["has_api_key"] is False
        assert "HTTP 500" in details["mint_problem"]
        assert "mk-1" not in (completed.stdout or "")
        stored = list(tmp_path.glob("*.secret"))
        assert len(stored) == 1
        record = json.loads(stored[0].read_text())
        # The dca token is the fallback credential and keeps its deadline.
        assert record["access_token"] == "dca-1"
        assert "api_key" not in record
        assert record["dca_expires_at_unix"] > 0


def test_denied_authorization_exits_nonzero_with_the_reason(tmp_path: Path) -> None:
    with _MetaServer([(400, {"error": "access_denied"})]) as server:
        completed = _run(tmp_path, _loopback_args(server))
        assert completed.returncode == 1
        assert "meta device authorization denied" in completed.stderr


def test_expired_code_exits_nonzero(tmp_path: Path) -> None:
    with _MetaServer([(400, {"error": "expired_token"})]) as server:
        completed = _run(tmp_path, _loopback_args(server))
        assert completed.returncode == 1
        assert "meta device code expired" in completed.stderr


def test_non_loopback_http_endpoints_are_refused(tmp_path: Path) -> None:
    completed = _run(
        tmp_path,
        [
            "--device-endpoint",
            "http://evil.example/device",
            "--token-endpoint",
            "http://evil.example/token",
        ],
    )
    assert completed.returncode == 1
    assert "must be an https URL" in completed.stderr

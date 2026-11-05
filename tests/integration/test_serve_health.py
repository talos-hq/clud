"""`clud serve` answers the liveness probe a remote client or monitor uses.

CLIProxyAPI registers an unauthenticated `GET|HEAD /healthz`
(internal/api/server_routes.go:53-64). clud answers the same path behind its
one client-token contract, so the probe must prove three things: a healthy
process answers `{"status":"ok"}` without dialing any provider, a HEAD probe
carries headers but no body, and a caller without the token is refused rather
than told the server is up.
"""

from __future__ import annotations

import http.client
import json
from pathlib import Path
from urllib.parse import urlsplit

import pytest

from tests import process

pytestmark = pytest.mark.integration


def test_serve_health_probe_is_token_gated_and_provider_free(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    client_token = "local-client-token-0123456789abcdef"
    provider_key = "claude-upstream-canary"
    env = mock_env.copy()
    env.update(
        {
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_TEST_SECRET_STORE_DIR": str(tmp_path / "vault"),
            "CLUD_SERVE_CLIENT_TOKEN": client_token,
            "ANTHROPIC_API_KEY": provider_key,
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
        }
    )
    # No fake upstream is configured on purpose: a probe that reached a provider
    # would be a defect, and this suite would hang instead of passing.
    env.pop("CLUD_TEST_UNIFIED_ANTHROPIC_UPSTREAM_URL", None)
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
        assert provider_key not in line
        address = urlsplit(line.strip().split("listening at ", 1)[1])
        assert address.hostname == "127.0.0.1"
        assert address.port is not None

        def probe(method: str, token: str | None) -> tuple[int, bytes]:
            connection = http.client.HTTPConnection(
                address.hostname, address.port, timeout=15
            )
            headers = {} if token is None else {"x-api-key": token}
            connection.request(method, "/healthz", headers=headers)
            response = connection.getresponse()
            data = response.read()
            connection.close()
            return response.status, data

        status, body = probe("GET", client_token)
        assert status == 200
        assert json.loads(body) == {"status": "ok"}
        assert client_token not in body.decode()
        assert provider_key not in body.decode()

        head_status, head_body = probe("HEAD", client_token)
        assert head_status == 200
        assert head_body == b""

        missing_status, missing_body = probe("GET", None)
        assert missing_status == 401
        assert b"ok" not in missing_body

        wrong_status, _ = probe("GET", "wrong-client-token")
        assert wrong_status == 401

        # The probe is a liveness door, not a second provider surface.
        unrouted = http.client.HTTPConnection(address.hostname, address.port, timeout=15)
        unrouted.request("POST", "/healthz", body="{}", headers={"x-api-key": client_token})
        unrouted_response = unrouted.getresponse()
        assert unrouted_response.status == 404
        unrouted_response.read()
        unrouted.close()
    finally:
        child.terminate()
        child.wait(timeout=30)

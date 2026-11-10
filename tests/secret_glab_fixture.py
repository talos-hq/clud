"""An isolated GitLab API for the real CLI credential-consumer acceptance test."""
from __future__ import annotations

import json
import os
import threading
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HOST = "secret-prompt-fixture.invalid"
TOKEN = "glpat-FAKE_NATIVE_GLAB_CREDENTIAL_CANARY"


def environment(root: Path, source: Path) -> dict[str, str]:
    # Never let a real account or proxy override this fixture's saved token.
    excluded = {"OAUTH_TOKEN", "CI_JOB_TOKEN", "API_PROTOCOL", "PROXY", "CHECK_UPDATE"}
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("GITLAB_", "GLAB_")) and key not in excluded}
    home = root / "home"
    home.mkdir()
    env.update({
        "HOME": str(home), "USERPROFILE": str(home),
        "GLAB_CONFIG_DIR": str(root / "glab-config"), "GLAB_CHECK_UPDATE": "false",
        "GLAB_DEBUG": "false", "GLAB_DEBUG_HTTP": "false", "GLAB_PROXY": "",
        "GIT_CONFIG_GLOBAL": str(root / "gitconfig"), "GIT_CONFIG_NOSYSTEM": "1",
        "NO_PROXY": "127.0.0.1,localhost", "no_proxy": "127.0.0.1,localhost",
        "CLUD_INTEGRATION_TESTS": "1", "CLUD_TEST_SECRET_PROMPT_FILE": str(source),
    })
    return env


def handler(accepted: list[str]):
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            supplied = self.headers.get("PRIVATE-TOKEN")
            authorized = supplied == TOKEN or self.headers.get("Authorization") == f"Bearer {TOKEN}"
            valid = authorized and self.path == "/api/v4/user"
            if valid:
                accepted.append(self.path)
            payload = {"id": 1, "username": "fixture-user", "name": "Fixture User"}
            if not valid:
                payload = {"message": "401 Unauthorized"}
            body = json.dumps(payload).encode()
            self.send_response(200 if valid else 401)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args) -> None:
            pass

    return Handler


@contextmanager
def gitlab_api():
    accepted: list[str] = []
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler(accepted))
    server.daemon_threads = True
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        yield f"127.0.0.1:{server.server_port}", accepted
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=5)
        assert not worker.is_alive(), "GitLab fixture server did not stop"

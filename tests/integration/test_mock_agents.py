"""Integration tests: verify clud correctly invokes mock claude/codex agents."""

from __future__ import annotations

import http.server
import json
import os
import re
import signal
import socket
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

import pytest

from tests import process
from tests._subprocess_helpers import is_ctrl_c_exit
from tests.metered_consent import consent_for

from ._daemon_helpers import (
    copy_launcher,
    describe_process_tree,
    kill_process,
    run_clud,
    tail_file,
    tail_text,
    wait_for_pids_to_exit,
    wait_for_tree_pids,
)

pytestmark = pytest.mark.integration

# 30s, matching `test_voice_mode` and `test_loop_stream_json`. The old 15s was
# the budget #994 names: it was missed by 0.2s in one observed run
# (`test_tool_shell_exit_reaps_leaked_client`, "timed out after 15s (waited
# 15.2s)") and by 1.6s in another (`test_subprocess_stdin_is_not_terminal_when
# _piped`, 16.6s), on runs whose diffs could not reach either test. A limit
# missed by a second is a statement about the runner, not the code.
#
# Widening rather than making it configurable, matching how the sibling
# deadlines were handled in 6e5634b and 40dd895: an env knob only helps
# someone who already knows to set it, and the failure mode here is a stranger
# reading a red lane.
_TIMEOUT = 30
_ANSI_RE = re.compile(r"\x1b(?:\[[^a-zA-Z]*[a-zA-Z]|\][^\x07]*\x07)")


def _strip_ansi(text: str) -> str:
    return _ANSI_RE.sub("", text)


def _run(
    clud: Path,
    *args: str,
    env: dict[str, str],
    input_data: str | None = None,
    cwd: Path | None = None,
) -> process.CompletedProcess[str]:
    # Issue #594: `ignore_cleanup_errors` so teardown never raises when Windows
    # still holds the just-executed clud.exe (Defender scan / lingering handle);
    # the temp dir is reclaimed later. `copy_launcher` covers the copy side.
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as temp_dir:
        launch = Path(temp_dir) / clud.name
        copy_launcher(clud, launch)
        # Issue #594: `run_clud` re-raises a timeout with the partial output,
        # so a stalled launch on the Windows lane says how far it got.
        return run_clud(
            [str(launch), *args],
            timeout=_TIMEOUT,
            # Launch tests that pick a metered provider consent to it
            # explicitly (subscription-first policy, #2529).
            env=consent_for(env, args),
            input=input_data,
            cwd=cwd,
        )


def _parse_agent_report(result: process.CompletedProcess[str]) -> dict:
    """Parse the JSON report from the mock agent.

    PTY output may contain ANSI escape sequences around the JSON.
    Extract the first valid JSON object from the cleaned output.
    """
    cleaned = _strip_ansi(result.stdout)
    for line in cleaned.splitlines():
        line = line.strip()
        if line.startswith("{"):
            return json.loads(line)
    return json.loads(cleaned)


def _wait_for_pid_log(path: Path, minimum: int, timeout: float = 10.0) -> list[int]:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.is_file():
            pids = [
                int(line)
                for line in path.read_text(encoding="utf-8").splitlines()
                if line.strip().isdigit()
            ]
            if len(pids) >= minimum:
                return pids
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {minimum} child pids in {path}")


class TestBackendSelection:
    """Verify clud selects the correct backend."""

    def test_default_is_claude(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert "claude" in report["program"].lower()

    def test_codex_flag(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "--codex", "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert "codex" in report["program"].lower()
        pairs = list(zip(report["args"], report["args"][1:], strict=False))
        assert pairs.count(("-m", "gpt-6-sol")) == 1
        assert pairs.count(("-c", 'model_reasoning_effort="low"')) == 1
        assert not any("terra" in arg or arg == "medium" for arg in report["args"])

    def test_claude_flag(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "--claude", "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert "claude" in report["program"].lower()

    @pytest.mark.skipif(sys.platform == "win32", reason="POSIX shell PATH-poisoning fixture")
    @pytest.mark.parametrize("use_daemon", [False, True])
    def test_child_invokes_launching_clud_not_path_stub(
        self,
        clud_binary: Path,
        mock_env: dict[str, str],
        tmp_path: Path,
        use_daemon: bool,
    ) -> None:
        stub_dir = tmp_path / "path-first"
        stub_dir.mkdir()
        marker = tmp_path / "stub-was-called"
        stub = stub_dir / "clud"
        stub.write_text(
            '#!/bin/sh\nprintf "called\\n" > "$CLUD_STUB_MARKER"\nprintf "STUB-CLUD 0.0.0\\n"\n',
            encoding="utf-8",
        )
        stub.chmod(0o755)
        env = dict(mock_env)
        env["PATH"] = f"{stub_dir}{os.pathsep}{env.get('PATH', '')}"
        env["CLUD_STUB_MARKER"] = str(marker)
        env["MOCK_AGENT_CLUD_EXE_PROBE"] = "1"
        args = [] if use_daemon else ["--no-daemon"]

        result = _run(clud_binary, *args, "--claude", "--subprocess", "-p", "test", env=env)
        assert result.returncode == 0, result.stderr
        report = _parse_agent_report(result)
        pinned = report["env"]["CLUD_EXE"]
        assert pinned
        assert Path(pinned).is_absolute()
        assert report["clud_exe_probe"]["exit_code"] == 0
        expected_version = run_clud([str(clud_binary), "--version"], timeout=10, env=mock_env)
        assert report["clud_exe_probe"]["stdout"].strip() == expected_version.stdout.strip()
        assert "STUB-CLUD" not in report["clud_exe_probe"]["stdout"]
        assert not marker.exists(), "the PATH-first stub intercepted an internal invocation"

    @pytest.mark.skipif(sys.platform != "win32", reason="Windows Job Object lifecycle")
    def test_tool_shell_exit_reaps_leaked_client(
        self,
        clud_binary: Path,
        mock_env: dict[str, str],
        tmp_path: Path,
    ) -> None:
        report_path = tmp_path / "tool-shell-probe.json"
        result = _run(
            clud_binary,
            "--no-daemon",
            "--codex",
            "--subprocess",
            "--mock-tool-shell-probe",
            str(report_path),
            env=mock_env,
        )

        assert result.returncode == 0, result.stderr
        report = json.loads(report_path.read_text(encoding="utf-8"))
        assert report["tool_shell_probe"]["reaped"] is True

    def test_codex_preserves_cwd(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "--codex", "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert report["cwd"] == str(Path.cwd())

    @pytest.mark.skipif(sys.platform != "win32", reason="Windows-only codex.cmd repro")
    def test_codex_cmd_wrapper_launches_noninteractive_exec(
        self, clud_binary: Path, mock_env_codex_cmd: dict[str, str]
    ) -> None:
        result = _run(
            clud_binary,
            "--no-daemon",
            "--codex",
            "-p",
            "hello from codex.cmd",
            env=mock_env_codex_cmd,
        )
        assert result.returncode == 0, f"stderr: {result.stderr}"
        report = _parse_agent_report(result)
        assert "exec" in report["args"]
        assert "--dangerously-bypass-approvals-and-sandbox" in report["args"]
        assert "hello from codex.cmd" in report["args"]

    def test_claude_preserves_cwd(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "--claude", "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert report["cwd"] == str(Path.cwd())

    def test_claude_preserves_explicit_launch_cwd(
        self, clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
    ) -> None:
        result = _run(clud_binary, "--claude", "-p", "hello", env=mock_env, cwd=tmp_path)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert report["cwd"] == str(tmp_path)


def _responses_sse(reply_index: int, input_tokens: int, cached_tokens: int) -> bytes:
    """Return a valid Responses stream with deterministic cache usage."""
    completed = json.dumps(
        {
            "type": "response.completed",
            "response": {
                "usage": {
                    "input_tokens": input_tokens,
                    "input_tokens_details": {"cached_tokens": cached_tokens},
                    "output_tokens": 2,
                }
            },
        },
        separators=(",", ":"),
    ).encode()
    return (
        b'event: response.created\ndata: {"type":"response.created"}\n\n'
        b'event: response.output_text.delta\ndata: {"type":"response.output_text.delta",'
        b'"output_index":0,"content_index":0,"delta":"bridged "}\n\n'
        + b'event: response.output_text.delta\ndata: {"type":"response.output_text.delta",'
        + b'"output_index":0,"content_index":0,"delta":"reply '
        + str(reply_index).encode()
        + b'"}\n\n'
        + b'event: response.output_text.done\ndata: {"type":"response.output_text.done",'
        + b'"output_index":0,"content_index":0}\n\n'
        + b"event: response.completed\ndata: "
        + completed
        + b"\n\n"
    )


class _FakeResponsesServer:
    """A deterministic OpenAI-Responses-shaped upstream.

    Issue #627 step 5 repointed the bridge's debug seam at this rather than at
    the phase-2 passthrough, so the end-to-end tests exercise real translation
    instead of proving only transport and auth.
    """

    def __init__(self) -> None:
        self._listener = socket.socket()
        self._listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._listener.bind(("127.0.0.1", 0))
        self._listener.listen(8)
        self._listener.settimeout(0.2)
        self.port = self._listener.getsockname()[1]
        self.requests: list[bytes] = []
        # Strict cache oracle: credit only a model-visible append to the
        # previous request for the same model/key. The ledger has counts only.
        self.cache_observations: list[dict[str, int | bool]] = []
        self._cache_inputs: dict[tuple[str, str], list[Any]] = {}
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def _serve(self) -> None:  # noqa: C901
        while not self._stop.is_set():
            try:
                connection, _ = self._listener.accept()
            except (TimeoutError, OSError):
                continue
            with connection:
                connection.settimeout(5)
                try:
                    raw = b""
                    while b"\r\n\r\n" not in raw:
                        chunk = connection.recv(4096)
                        if not chunk:
                            break
                        raw += chunk
                    head, _, rest = raw.partition(b"\r\n\r\n")
                    length = 0
                    for line in head.split(b"\r\n"):
                        name, _, value = line.partition(b":")
                        if name.lower() == b"content-length":
                            length = int(value.strip())
                    while len(rest) < length:
                        chunk = connection.recv(4096)
                        if not chunk:
                            break
                        rest += chunk
                    self.requests.append(head + b"\r\n\r\n" + rest)
                    payload = json.loads(rest)
                    model = str(payload.get("model", ""))
                    cache_key = str(payload.get("prompt_cache_key", ""))
                    input_items = payload.get("input", [])
                    if not isinstance(input_items, list):
                        input_items = []
                    cache_slot = (model, cache_key)
                    previous = self._cache_inputs.get(cache_slot)
                    prefix_is_stable = (
                        previous is not None and input_items[: len(previous)] == previous
                    )
                    input_tokens = len(json.dumps(input_items, separators=(",", ":")).encode())
                    cached_tokens = (
                        len(json.dumps(previous, separators=(",", ":")).encode())
                        if prefix_is_stable
                        else 0
                    )
                    self._cache_inputs[cache_slot] = input_items
                    self.cache_observations.append(
                        {
                            "input_tokens": input_tokens,
                            "cached_tokens": cached_tokens,
                            "cache_hit": prefix_is_stable,
                        }
                    )
                    response = _responses_sse(len(self.requests), input_tokens, cached_tokens)
                    connection.sendall(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n"
                        b"Content-Length: %d\r\nConnection: close\r\n\r\n%s"
                        % (len(response), response)
                    )
                except OSError:
                    continue

    def close(self) -> None:
        self._stop.set()
        self._thread.join(timeout=5)
        self._listener.close()


class _FakeAnthropicServer:
    """A minimal Messages-shaped upstream for unified proxy routes.

    It deliberately records raw test requests only in this fixture. Assertions
    project the wire contract and prove that the bridge did not turn a proxy
    route into a Codex Responses request or leak another route's credential.
    """

    def __init__(self, marker: str) -> None:
        self.marker = marker
        self._listener = socket.socket()
        self._listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._listener.bind(("127.0.0.1", 0))
        self._listener.listen(8)
        self._listener.settimeout(0.2)
        self.port = self._listener.getsockname()[1]
        self.requests: list[bytes] = []
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def _serve(self) -> None:
        while not self._stop.is_set():
            try:
                connection, _ = self._listener.accept()
            except (TimeoutError, OSError):
                continue
            with connection:
                connection.settimeout(5)
                try:
                    raw = b""
                    while b"\r\n\r\n" not in raw:
                        chunk = connection.recv(4096)
                        if not chunk:
                            break
                        raw += chunk
                    head, _, rest = raw.partition(b"\r\n\r\n")
                    length = 0
                    for line in head.split(b"\r\n"):
                        name, _, value = line.partition(b":")
                        if name.lower() == b"content-length":
                            length = int(value.strip())
                    while len(rest) < length:
                        chunk = connection.recv(4096)
                        if not chunk:
                            break
                        rest += chunk
                    self.requests.append(head + b"\r\n\r\n" + rest)
                    response = json.dumps(
                        {
                            "id": f"fake-{self.marker}-{len(self.requests)}",
                            "type": "message",
                            "role": "assistant",
                            "model": "fake-model",
                            "content": [{"type": "text", "text": f"fake {self.marker}"}],
                            "stop_reason": "end_turn",
                            "usage": {"input_tokens": 3, "output_tokens": 2},
                        },
                        separators=(",", ":"),
                    ).encode()
                    connection.sendall(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                        b"Content-Length: %d\r\nConnection: close\r\n\r\n%s"
                        % (len(response), response)
                    )
                except OSError:
                    continue

    def close(self) -> None:
        self._stop.set()
        self._thread.join(timeout=5)
        self._listener.close()


class _ModelDocumentServer:
    """Serve one published-document fixture over the same HTTP path as Pages."""

    def __init__(self, document: dict[str, Any]) -> None:
        body = json.dumps(document).encode()

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, format: str, *args: object) -> None:
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_port}/models/manifest.json"

    def close(self) -> None:
        self.server.shutdown()
        self.thread.join(timeout=5)
        self.server.server_close()


@pytest.fixture
def fake_responses():
    server = _FakeResponsesServer()
    try:
        yield server
    finally:
        server.close()


@pytest.fixture
def fake_anthropic_upstreams():
    servers = {name: _FakeAnthropicServer(name) for name in ("claude", "deepseek", "openrouter")}
    try:
        yield servers
    finally:
        for server in servers.values():
            server.close()


def _drop_scrub_lists(value: object) -> object:
    """Return a dry-run plan without its `scrub` key lists."""
    if isinstance(value, dict):
        return {k: _drop_scrub_lists(v) for k, v in value.items() if k != "scrub"}
    if isinstance(value, list):
        return [_drop_scrub_lists(v) for v in value]
    return value


class TestCodexBridgeForeground:
    """Issue #626: Codex provider through the Claude foreground harness."""

    @pytest.mark.parametrize(
        ("model", "wire", "discovery"),
        [
            ("codex-sol-6.1", "gpt-6.1-sol", "clud-claude-codex-sol-6-1"),
            ("sol-6.1", "gpt-6.1-sol", "clud-claude-codex-sol-6-1"),
            ("gpt-6.1-sol", "gpt-6.1-sol", "clud-claude-codex-sol-6-1"),
            ("codex-terra-6.1", "gpt-6.1-terra", "clud-claude-codex-terra-6-1"),
            ("terra-6.1", "gpt-6.1-terra", "clud-claude-codex-terra-6-1"),
            ("gpt-6.1-terra", "gpt-6.1-terra", "clud-claude-codex-terra-6-1"),
        ],
    )
    def test_6_1_launch_reaches_the_selected_wire_model(
        self, clud_binary: Path, mock_env: dict[str, str], tmp_path: Path,
        fake_responses: _FakeResponsesServer, model: str, wire: str, discovery: str,
    ) -> None:
        env = mock_env.copy()
        env["CLUD_INTEGRATION_TESTS"] = "1"
        env["CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL"] = fake_responses.base_url
        dry = _run(
            clud_binary, "--dry-run", "--codex", "--harness", "claude",
            "--model", model, "--effort", "high", "-p", "hello", env=env,
        )
        assert dry.returncode == 0, dry.stderr
        plan = json.loads(dry.stdout)
        model_index = plan["command"].index("--model")
        assert plan["command"][model_index + 1] == discovery
        result = _run(
            clud_binary, "--codex", "--harness", "claude", "--subprocess",
            "--model", model, "--effort", "high", "-p", "hello", "--",
            "--mock-codex-bridge-probe", str(tmp_path / "sol-6-1-probe.json"), env=env,
        )
        assert result.returncode == 0, result.stderr
        assert len(fake_responses.requests) == 1
        _, _, body = fake_responses.requests[0].partition(b"\r\n\r\n")
        sent = json.loads(body)
        assert sent["model"] == wire
        assert sent["reasoning"]["effort"] == "high"

    @pytest.mark.skipif(sys.platform == "win32", reason="fixture uses a POSIX executable script")
    @pytest.mark.parametrize(
        ("available", "expected", "source", "page_two"),
        [
            (["gpt-7-sol", "gpt-7-luna"], "gpt-7-sol", "published", False),
            (["gpt-6-sol", "gpt-6-luna"], "gpt-6-sol", "built_in", False),
            (["gpt-6-sol", "gpt-6.1-sol", "gpt-6-luna"], "gpt-6.1-sol", "built_in", False),
            (["gpt-7-sol", "gpt-6-luna"], "gpt-7-sol", "published", True),
        ],
    )
    def test_published_sol_or_fallback_reaches_billed_bridge_turn(
        self,
        clud_binary: Path,
        mock_env: dict[str, str],
        tmp_path: Path,
        fake_responses: _FakeResponsesServer,
        available: list[str],
        expected: str,
        source: str,
        page_two: bool,
    ) -> None:
        """#1476: stable Claude argv, account-checked wire ID, and billed low effort."""
        document = {
            "schema_version": 1,
            "source": "openrouter+models.dev-merge",
            "checked_at": "2026-09-26T21:00:00Z",
            "trigger": "manual",
            "families": {"sol": "gpt-7-sol", "luna": "gpt-7-luna"},
            "defaults": {"codex": {"model": "gpt-7-sol", "effort": "low"}},
        }
        server = _ModelDocumentServer(document)
        bin_dir = tmp_path / "fake-codex-bin"
        bin_dir.mkdir()
        app_server = bin_dir / "codex"
        rows = [
            {"id": model, "supportedReasoningEfforts": [{"reasoningEffort": "low"}]}
            for model in available
        ]
        script = (
            f"#!{sys.executable}\n"
            "import json, sys\n"
            "for line in sys.stdin:\n"
            "    frame = json.loads(line)\n"
            "    if frame.get('method') == 'model/list':\n"
            f"        if {page_two!r} and frame.get('params', {{}}).get('cursor') != 'second':\n"
            "            result = {'data': [], 'nextCursor': 'second'}\n"
            "        else:\n"
            f"            result = {{'data': {rows!r}, 'nextCursor': None}}\n"
            "        print(json.dumps({'id': frame['id'], 'result': result}), flush=True)\n"
            "        if result['nextCursor'] is None:\n"
            "            break\n"
        )
        app_server.write_text(script, encoding="utf-8")
        app_server.chmod(0o755)
        env = mock_env.copy()
        env.update(
            {
                "PATH": str(bin_dir) + os.pathsep + env["PATH"],
                "CLUD_MODEL_MANIFEST_URL": server.url,
                "CLUD_INTEGRATION_TESTS": "1",
                "CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL": fake_responses.base_url,
            }
        )
        try:
            dry = _run(
                clud_binary, "--dry-run", "--codex", "--harness", "claude", "-p", "hello", env=env
            )
            assert dry.returncode == 0, dry.stderr
            plan = json.loads(dry.stdout)
            assert plan["model_selection"]["wire_model"] == expected
            assert plan["model_selection"]["effort"] == "low"
            assert plan["codex_model_source"] == source
            if expected == "gpt-6.1-sol":
                invalid = _run(
                    clud_binary, "--dry-run", "--codex", "--harness", "claude",
                    "--effort", "none", "-p", "hello", env=env,
                )
                assert invalid.returncode != 0
                assert "none" in invalid.stderr
            native = _run(clud_binary, "--dry-run", "--codex", "-p", "hello", env=env)
            assert native.returncode == 0, native.stderr
            native_plan = json.loads(native.stdout)
            assert native_plan["model_selection"]["wire_model"] == expected
            assert native_plan["codex_model_source"] == source
            assert native_plan["command"].count(expected) == 1
            assert 'model_reasoning_effort="low"' in native_plan["command"]
            model_index = plan["command"].index("--model")
            assert plan["command"][model_index + 1] == "clud-claude-codex-sol"

            probe_path = tmp_path / "published-sol-probe.json"
            launched = _run(
                clud_binary,
                "--codex",
                "--harness",
                "claude",
                "--subprocess",
                "-p",
                "hello",
                "--",
                "--mock-codex-bridge-probe",
                str(probe_path),
                env=env,
            )
            assert launched.returncode == 0, launched.stderr
            probe = json.loads(probe_path.read_text(encoding="utf-8"))
            assert probe["status"] == 200, probe
            assert len(fake_responses.requests) == 1
            _, _, body = fake_responses.requests[0].partition(b"\r\n\r\n")
            billed = json.loads(body)
            assert billed["model"] == expected
            assert billed["reasoning"]["effort"] == "low"
        finally:
            server.close()

    def test_codex_cache_identity_is_stable_through_the_subprocess_harness(
        self,
        clud_binary: Path,
        mock_env: dict[str, str],
        tmp_path: Path,
        fake_responses: _FakeResponsesServer,
    ) -> None:
        env = mock_env.copy()
        env["CLUD_INTEGRATION_TESTS"] = "1"
        env["CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL"] = fake_responses.base_url
        probe_path = tmp_path / "codex-cache-identity-probe.json"
        result = _run(
            clud_binary,
            "--codex",
            "--harness",
            "claude",
            "--subprocess",
            "-p",
            "cache identity",
            "--",
            "--mock-codex-cache-identity-probe",
            str(probe_path),
            env=env,
        )
        assert result.returncode == 0, result.stderr
        probe = json.loads(probe_path.read_text(encoding="utf-8"))
        assert probe == {
            "attempted": True,
            "loopback": True,
            "statuses": [200] * 8,
            "bridged_reply_count": 8,
            "error": None,
        }
        assert "cache-session" not in json.dumps(probe)

        requests = fake_responses.requests
        assert len(requests) == 8

        def header(raw: bytes, name: bytes) -> bytes:
            head, _, _ = raw.partition(b"\r\n\r\n")
            for line in head.split(b"\r\n"):
                candidate, _, value = line.partition(b":")
                if candidate.lower() == name.lower():
                    return value.strip()
            raise AssertionError(f"missing {name!r} in {head!r}")

        identities = []
        bodies = []
        for raw in requests:
            head, _, body = raw.partition(b"\r\n\r\n")
            sent = json.loads(body)
            identity = (
                header(raw, b"session-id"),
                header(raw, b"thread-id"),
                header(raw, b"x-client-request-id"),
                sent["prompt_cache_key"].encode(),
            )
            assert len(set(identity)) == 1
            identities.append(identity[0])
            bodies.append(sent)
            assert b"cache-session" not in head + body
            assert b"cache-agent-a" not in head + body
            assert b"cache-agent-b" not in head + body

        for turn in range(1, 5):
            assert identities[turn] == identities[0]
            assert (
                bodies[turn]["input"][: len(bodies[turn - 1]["input"])] == bodies[turn - 1]["input"]
            )
        assert identities[5] == identities[6]
        assert bodies[6]["input"][: len(bodies[5]["input"])] == bodies[5]["input"]
        assert len({identities[0], identities[5], identities[7]}) == 3

        observations = fake_responses.cache_observations
        assert [entry["cache_hit"] for entry in observations] == [
            False,
            True,
            True,
            True,
            True,
            False,
            True,
            False,
        ]
        total_uncached = sum(
            entry["input_tokens"] - entry["cached_tokens"] for entry in observations
        )
        # A request-scoped key makes every replay cold (22 copies of this
        # fixture's 16 KiB prefix). The stable identity may pay only three
        # cold seeds plus five append-only suffixes.
        assert total_uncached < 10 * 16_384

    def test_unified_route_matrix_through_one_mocked_claude_session(  # noqa: C901
        self,
        clud_binary: Path,
        mock_env: dict[str, str],
        tmp_path: Path,
        fake_responses: _FakeResponsesServer,
        fake_anthropic_upstreams: dict[str, _FakeAnthropicServer],
    ) -> None:
        """#1226: provider-shaped foreground coverage, not unit-only routing."""
        env = mock_env.copy()
        env.update(
            {
                "CLUD_INTEGRATION_TESTS": "1",
                "CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL": fake_responses.base_url,
                "CLUD_TEST_UNIFIED_ANTHROPIC_UPSTREAM_URL": (
                    fake_anthropic_upstreams["claude"].base_url
                ),
                "CLUD_TEST_UNIFIED_DEEPSEEK_UPSTREAM_URL": (
                    fake_anthropic_upstreams["deepseek"].base_url
                ),
                "CLUD_TEST_UNIFIED_OPENROUTER_UPSTREAM_URL": (
                    fake_anthropic_upstreams["openrouter"].base_url
                ),
                "ANTHROPIC_API_KEY": "ambient-key-must-not-reach-routed-fakes",
            }
        )
        probe_path = tmp_path / "unified-route-probe.json"
        result = _run(
            clud_binary,
            "--unified",
            "--harness",
            "claude",
            "--subprocess",
            "-p",
            "unified route matrix",
            "--",
            "--mock-unified-route-probe",
            str(probe_path),
            env=env,
        )
        assert result.returncode == 0, result.stderr
        probe = json.loads(probe_path.read_text(encoding="utf-8"))
        assert probe == {
            "attempted": True,
            "loopback": True,
            "statuses": [200] * 10,
            "response_count": 10,
            "error": None,
        }
        assert "unified-session" not in json.dumps(probe)

        def split(raw: bytes) -> tuple[bytes, dict[str, Any]]:
            head, _, body = raw.partition(b"\r\n\r\n")
            return head, json.loads(body)

        def header_value(head: bytes, name: bytes) -> bytes | None:
            for line in head.split(b"\r\n"):
                candidate, _, value = line.partition(b":")
                if candidate.lower() == name.lower():
                    return value.strip()
            return None

        # Native Claude, DeepSeek, and OpenRouter remain Messages-shaped and
        # retain the caller-owned cache-control block byte-for-byte. Codex is
        # separately Responses-shaped and covered by its identity assertions.
        expected = {
            "claude": (2, ["claude-opus-4-1", "claude-opus-4-1"]),
            "deepseek": (2, ["deepseek-flash[1m]", "deepseek-flash[1m]"]),
            "openrouter": (2, ["~anthropic/claude-sonnet-latest"] * 2),
        }
        for route, (count, models) in expected.items():
            requests = fake_anthropic_upstreams[route].requests
            assert len(requests) == count
            parsed = [split(raw) for raw in requests]
            assert all(head.startswith(b"POST /v1/messages HTTP/1.1") for head, _ in parsed)
            assert [body["model"] for _, body in parsed] == models
            for head, body in parsed:
                assert body["system"][0]["cache_control"] == {"type": "ephemeral"}
                assert "prompt_cache_key" not in body
                assert b"session-id:" not in head.lower()
                assert b"thread-id:" not in head.lower()
                assert b"ambient-key-must-not-reach-routed-fakes" not in head
                assert header_value(head, b"x-clud-gateway-token") is None
                if route == "claude":
                    assert b"clud-test-deepseek-key" not in head
                    assert b"clud-test-openrouter-key" not in head

        for route, fixture_key in {
            "deepseek": b"clud-test-deepseek-key",
            "openrouter": b"clud-test-openrouter-key",
        }.items():
            for raw in fake_anthropic_upstreams[route].requests:
                head, _ = split(raw)
                assert header_value(head, b"authorization") == b"Bearer " + fixture_key
                other_key = (
                    b"clud-test-openrouter-key"
                    if route == "deepseek"
                    else b"clud-test-deepseek-key"
                )
                assert other_key not in head

        assert len(fake_responses.requests) == 4
        codex_bodies = []
        for raw in fake_responses.requests:
            head, body = split(raw)
            assert head.startswith(b"POST /v1/responses HTTP/1.1")
            assert body["model"] == "gpt-5.6-terra"
            assert "messages" not in body
            codex_bodies.append(body)
            assert b"unified-session" not in raw
            assert b"unified-agent-codex" not in raw

        # The two main Codex turns are append-only under one derived identity;
        # the child turn is deliberately isolated from that cache scope. After
        # DeepSeek/OpenRouter the final main turn retains identity but is a
        # fresh route epoch, so it cannot replay Codex-private response items.
        assert codex_bodies[1]["input"][: len(codex_bodies[0]["input"])] == codex_bodies[0]["input"]
        assert codex_bodies[0]["prompt_cache_key"] == codex_bodies[1]["prompt_cache_key"]
        assert codex_bodies[2]["prompt_cache_key"] != codex_bodies[1]["prompt_cache_key"]
        assert codex_bodies[3]["prompt_cache_key"] == codex_bodies[1]["prompt_cache_key"]
        final_input = json.dumps(codex_bodies[3]["input"])
        assert "unified-route-turn-2" not in final_input
        assert "unified-route-turn-3" not in final_input
        assert [entry["cache_hit"] for entry in fake_responses.cache_observations] == [
            False,
            True,
            False,
            False,
        ]

    @pytest.mark.parametrize("launch_mode", ["--subprocess", "--pty"])
    def test_cross_route_translates_upstream_and_closes_listener(
        self,
        clud_binary: Path,
        mock_env: dict[str, str],
        tmp_path: Path,
        launch_mode: str,
        fake_responses: _FakeResponsesServer,
    ) -> None:
        env = mock_env.copy()
        env["ANTHROPIC_API_KEY"] = "ambient-secret-that-must-not-leak"
        # The seam is gated on a debug build *and* this flag; release builds
        # ignore it entirely.
        env["CLUD_INTEGRATION_TESTS"] = "1"
        env["CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL"] = fake_responses.base_url
        probe_path = tmp_path / "codex-bridge-probe.json"
        agent_report_path = tmp_path / "codex-bridge-agent-report.json"
        result = _run(
            clud_binary,
            "--codex",
            "--harness",
            "claude",
            launch_mode,
            "-p",
            "hello",
            "--",
            "--mock-codex-bridge-probe",
            str(probe_path),
            "--mock-report-file",
            str(agent_report_path),
            env=env,
            input_data="\x1b[1;1R" if launch_mode == "--pty" else None,
        )
        assert result.returncode == 0, result.stderr
        report = json.loads(agent_report_path.read_text(encoding="utf-8"))
        assert "claude" in report["program"].lower()
        assert set(report["env"]) == {
            "IN_CLUD",
            "CLUD_EXE",
            "RUNNING_PROCESS_ORIGINATOR",
            "ANTHROPIC_BASE_URL_PRESENT",
            "ANTHROPIC_AUTH_TOKEN_PRESENT",
            "ANTHROPIC_API_KEY_PRESENT",
            "API_TIMEOUT_MS",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
            "WEZTERM_UNIX_SOCKET",
            "UV_CACHE_DIR",
        }
        # The mock always reports this key; outside a Kitty pane it is unset.
        assert report["env"]["WEZTERM_UNIX_SOCKET"] is None
        assert report["env"]["ANTHROPIC_BASE_URL_PRESENT"] is True
        assert report["env"]["ANTHROPIC_AUTH_TOKEN_PRESENT"] is True
        assert report["env"]["ANTHROPIC_API_KEY_PRESENT"] is False
        assert report["env"]["API_TIMEOUT_MS"] == "3000000"
        assert report["env"]["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"] is None
        assert report["env"]["CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"] == "1"
        assert report["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"] == "1050000"
        probe = json.loads(probe_path.read_text(encoding="utf-8"))
        assert probe == report["codex_bridge_probe"]
        assert set(probe) == {
            "attempted",
            "loopback",
            "port",
            "status",
            "bridged_reply_received",
            "error",
        }
        assert probe["attempted"] is True
        assert probe["loopback"] is True
        assert probe["status"] == 200
        assert probe["bridged_reply_received"] is True
        assert probe["error"] is None

        # The bridge must have spoken the Responses protocol upstream, in the
        # translated shape -- not passed the Anthropic body through.
        assert len(fake_responses.requests) == 1, "expected exactly one paid-shape request"
        upstream_request = fake_responses.requests[0]
        assert upstream_request.startswith(b"POST /v1/responses HTTP/1.1")
        head, _, body = upstream_request.partition(b"\r\n\r\n")
        sent = json.loads(body)
        # The billed default, end to end through a real launch. Asserted as a
        # literal on purpose: this is the one place the model id is observed
        # after travelling the whole path, so it is the assertion that would
        # catch an unintended change to what the user is charged.
        assert sent["model"] == "gpt-6-sol"
        assert sent["reasoning"]["effort"] == "low"
        assert sent["stream"] is True
        assert sent["input"][0]["content"][0]["type"] == "input_text"
        assert "messages" not in sent, "Anthropic request shape leaked upstream"
        # Neither the ambient key nor the bridge's own bearer may travel on.
        assert b"ambient-secret-that-must-not-leak" not in upstream_request
        assert b"x-api-key" not in head.lower()
        assert "ambient-secret-that-must-not-leak" not in result.stdout
        assert "ambient-secret-that-must-not-leak" not in result.stderr
        address = ("127.0.0.1", probe["port"])
        try:
            connection = socket.create_connection(address, timeout=0.2)
        except ConnectionRefusedError:
            pass
        except TimeoutError:
            # Some Windows firewall policies drop closed-loopback SYN packets
            # instead of returning WSAECONNREFUSED. Rebinding the exact address
            # distinguishes that host behavior from a surviving listener.
            with socket.socket() as rebound:
                rebound.bind(address)
        else:
            connection.close()
            pytest.fail("bridge listener survived foreground process exit")

    def test_sol_selection_survives_single_iteration_claude_loop(
        self,
        clud_binary: Path,
        mock_env: dict[str, str],
        tmp_path: Path,
        fake_responses: _FakeResponsesServer,
    ) -> None:
        """Issue #955: the paid loop request must reach the selected Sol row."""
        env = mock_env.copy()
        env["CLUD_INTEGRATION_TESTS"] = "1"
        env["CLUD_TEST_CODEX_BRIDGE_UPSTREAM_URL"] = fake_responses.base_url
        probe_path = tmp_path / "sol-loop-probe.json"
        result = _run(
            clud_binary,
            "--codex",
            "--harness",
            "claude",
            "--subprocess",
            "--model",
            "codex-sol",
            "--effort",
            "low",
            "loop",
            "--loop-count",
            "1",
            "--no-done",
            "reply once",
            "--",
            "--mock-codex-bridge-probe",
            str(probe_path),
            env=env,
        )
        assert result.returncode == 0, result.stderr
        assert len(fake_responses.requests) == 1
        _, _, body = fake_responses.requests[0].partition(b"\r\n\r\n")
        sent = json.loads(body)
        assert sent["model"] == "gpt-6-sol"
        assert sent["reasoning"]["effort"] == "low"

    @pytest.mark.parametrize("provider_flag", ["--claude", "--codex"])
    def test_native_routes_receive_no_bridge_overlay(
        self,
        clud_binary: Path,
        mock_env: dict[str, str],
        tmp_path: Path,
        provider_flag: str,
    ) -> None:
        env = mock_env.copy()
        for key in (
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ):
            env.pop(key, None)
        probe_path = tmp_path / f"native-{provider_flag[2:]}.json"
        result = _run(
            clud_binary,
            provider_flag,
            "--subprocess",
            "-p",
            "hello",
            "--",
            "--mock-codex-bridge-probe",
            str(probe_path),
            env=env,
        )
        assert result.returncode == 0, result.stderr
        report = _parse_agent_report(result)
        assert report["env"]["ANTHROPIC_BASE_URL_PRESENT"] is False
        assert report["env"]["ANTHROPIC_AUTH_TOKEN_PRESENT"] is False
        assert report["env"]["ANTHROPIC_API_KEY_PRESENT"] is False
        assert report["codex_bridge_probe"]["attempted"] is False

    def test_cross_route_dry_run_starts_no_bridge_and_emits_no_secret(
        self, clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
    ) -> None:
        probe_path = tmp_path / "dry-run-probe.json"
        result = _run(
            clud_binary,
            "--codex",
            "--harness",
            "claude",
            "--dry-run",
            "-p",
            "hello",
            "--",
            "--mock-codex-bridge-probe",
            str(probe_path),
            env=mock_env,
        )
        assert result.returncode == 0, result.stderr
        assert not probe_path.exists()
        # The plan names the keys it scrubs; naming a key is not emitting it.
        plan = _drop_scrub_lists(json.loads(result.stdout))
        combined = json.dumps(plan) + result.stderr
        assert "ANTHROPIC_AUTH_TOKEN" not in combined
        assert "Bearer " not in combined


class TestYoloMode:
    """Verify YOLO mode (--dangerously-skip-permissions) injection."""

    def test_yolo_injected_by_default(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert "--dangerously-skip-permissions" in report["args"]

    def test_safe_mode_no_yolo(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "--safe", "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert "--dangerously-skip-permissions" not in report["args"]


class TestPromptDelivery:
    """Verify prompts are correctly delivered to the backend."""

    def test_prompt_flag(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "-p", "hello world", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert "-p" in report["args"]
        assert "hello world" in report["args"]

    def test_message_flag(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "-m", "fix the bug", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert "-m" in report["args"]
        assert "fix the bug" in report["args"]


class TestSessionManagement:
    """Verify continue/resume flags are forwarded."""

    def test_continue_flag(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "-c", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert "--continue" in report["args"]


class TestModelSelection:
    """Verify model preference is forwarded."""

    def test_model_flag(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "--model", "opus", "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert "--model" in report["args"]
        assert "opus" in report["args"]


class TestLaunchMode:
    """Verify launch-mode selection and overrides."""

    def test_dry_run_defaults_to_subprocess(
        self, clud_binary: Path, mock_env: dict[str, str]
    ) -> None:
        result = _run(clud_binary, "--dry-run", "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = json.loads(result.stdout)
        assert report["launch_mode"] == "subprocess"

    def test_dry_run_pty_override(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "--dry-run", "--pty", "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = json.loads(result.stdout)
        assert report["launch_mode"] == "pty"


class TestExitCodePropagation:
    """Verify clud propagates the backend's exit code."""

    def test_exit_code_zero(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "-p", "hello", env=mock_env)
        assert result.returncode == 0

    def test_exit_code_nonzero(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "-p", "hello", "--", "--mock-exit-code", "42", env=mock_env)
        assert result.returncode == 42


class TestCommandPrompts:
    """Verify special commands inject the expected prompt text."""

    def test_up_command(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "up", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        idx = report["args"].index("-p")
        prompt = report["args"][idx + 1]
        assert "lint" in prompt.lower()
        assert "codeup" in prompt.lower()
        assert "<your one-line summary>" in prompt

    def test_up_with_message(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "up", "-m", "bump version", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        idx = report["args"].index("-p")
        prompt = report["args"][idx + 1]
        assert 'codeup -m "bump version"' in prompt
        assert "<your one-line summary>" not in prompt

    def test_up_with_publish(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "up", "--publish", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        idx = report["args"].index("-p")
        prompt = report["args"][idx + 1]
        assert "codeup" in prompt
        assert "-p" in prompt.split("codeup")[1]

    def test_up_with_message_and_publish(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "up", "-m", "release v2", "--publish", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        idx = report["args"].index("-p")
        prompt = report["args"][idx + 1]
        assert 'codeup -m "release v2" -p' in prompt

    def test_rebase_command(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "rebase", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        idx = report["args"].index("-p")
        prompt = report["args"][idx + 1]
        assert "git fetch" in prompt
        assert "rebase" in prompt.lower()

    def test_fix_command(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "fix", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        idx = report["args"].index("-p")
        prompt = report["args"][idx + 1]
        assert "linting" in prompt.lower()
        assert "unit tests" in prompt.lower()

    def test_fix_with_github_url(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        url = "https://github.com/user/repo/actions/runs/123"
        result = _run(clud_binary, "fix", url, env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        idx = report["args"].index("-p")
        prompt = report["args"][idx + 1]
        assert url in prompt
        assert "gh run view" in prompt
        assert "lint-test" in prompt


class TestEnvTracking:
    """Verify clud injects tracking env vars into the child process."""

    def test_session_child_does_not_inherit_clud_uv_cache(
        self, clud_binary: Path, mock_env: dict[str, str]
    ) -> None:
        # #1964: only bundled tools get ~/.clud/cache/uv; user commands in a
        # session keep uv's own default cache.
        env = {k: v for k, v in mock_env.items() if k != "UV_CACHE_DIR"}
        result = _run(clud_binary, "-p", "hello", env=env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert report["env"]["UV_CACHE_DIR"] is None

    def test_user_uv_cache_dir_passes_through(
        self, clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
    ) -> None:
        mine = str(tmp_path / "my-uv-cache")
        result = _run(clud_binary, "-p", "hello", env={**mock_env, "UV_CACHE_DIR": mine})
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert report["env"]["UV_CACHE_DIR"] == mine

    def test_in_clud_set(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert report["env"]["IN_CLUD"] == "1"

    def test_originator_set(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        originator = report["env"]["RUNNING_PROCESS_ORIGINATOR"]
        assert originator is not None
        assert originator.startswith("CLUD:")
        pid_str = originator.split(":")[1]
        assert pid_str.isdigit()


class TestFlagForwarding:
    """Verify unknown flags are forwarded to the backend."""

    def test_unknown_flag(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "--some-unknown-flag", "-p", "hello", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert "--some-unknown-flag" in report["args"]

    def test_passthrough_after_separator(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, "-p", "hello", "--", "--verbose", "--debug", env=mock_env)
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert "--verbose" in report["args"]
        assert "--debug" in report["args"]


class TestPipeMode:
    """Verify piped stdin is delivered to the backend."""

    def test_pipe_input(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(clud_binary, env=mock_env, input_data="piped prompt\n")
        assert result.returncode == 0
        report = _parse_agent_report(result)
        assert "-p" in report["args"]
        assert "piped prompt" in report["args"]


class TestLoopMode:
    """Verify loop mode runs multiple iterations."""

    def test_loop_iterations(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        # --no-done: this test only verifies the loop runs N times;
        # without it, the DONE-marker contract would make exit code 2 (not
        # converged) the expected outcome when no marker is written.
        result = _run(
            clud_binary,
            "loop",
            "--loop-count",
            "3",
            "--no-done",
            "do stuff",
            env=mock_env,
        )
        assert result.returncode == 0
        # `clud loop` against the claude backend now injects
        # `--output-format stream-json --verbose` in subprocess mode and
        # routes the child's stdout through the renderer, so the
        # mock-agent's own JSON report no longer arrives on the parent's
        # stdout verbatim. Count iterations via the `[clud] iteration N/3`
        # banner clud itself prints on stderr, which the renderer doesn't
        # touch.
        cleaned_stderr = _strip_ansi(result.stderr)
        iter_lines = [
            line
            for line in cleaned_stderr.splitlines()
            # Match the start-of-iteration banner specifically (it has the
            # form `[clud] iteration N/M`), not the per-iteration failure
            # diagnostic (`[clud] iteration N failed with exit code X`).
            if re.match(r"\[clud\] iteration \d+/\d+", line.strip())
        ]
        assert len(iter_lines) == 3, (
            f"expected 3 iteration banners, got {len(iter_lines)}: {iter_lines!r}"
        )
        assert "[clud] iteration 1/3" in cleaned_stderr
        assert "[clud] iteration 2/3" in cleaned_stderr
        assert "[clud] iteration 3/3" in cleaned_stderr

    def test_loop_stops_on_failure(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        result = _run(
            clud_binary,
            "loop",
            "--loop-count",
            "5",
            "task",
            "--",
            "--mock-exit-code",
            "1",
            env=mock_env,
        )
        assert result.returncode == 1
        # Same rationale as `test_loop_iterations`: iteration banners on
        # stderr are the post-renderer way to count iterations. We expect
        # exactly one because the failure aborts the loop.
        cleaned_stderr = _strip_ansi(result.stderr)
        iter_lines = [
            line
            for line in cleaned_stderr.splitlines()
            # Match the start-of-iteration banner specifically (it has the
            # form `[clud] iteration N/M`), not the per-iteration failure
            # diagnostic (`[clud] iteration N failed with exit code X`).
            if re.match(r"\[clud\] iteration \d+/\d+", line.strip())
        ]
        assert len(iter_lines) == 1, (
            f"expected loop to stop after iteration 1, got banners: {iter_lines!r}"
        )
        assert "[clud] iteration 1/5" in cleaned_stderr

    # ---- Issue #48: `clud --codex loop "..."` must work against codex ----

    def test_codex_loop_iterations(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        """`clud --codex loop ...` drives the codex backend for N iterations."""
        result = _run(
            clud_binary,
            "--no-daemon",
            "--codex",
            "loop",
            "--loop-count",
            "3",
            "--no-done",
            "do stuff",
            env=mock_env,
        )
        assert result.returncode == 0, f"stderr: {result.stderr}"
        cleaned = _strip_ansi(result.stdout)
        json_lines = [line.strip() for line in cleaned.splitlines() if line.strip().startswith("{")]
        assert len(json_lines) == 3
        for line in json_lines:
            report = json.loads(line)
            # Each iteration invoked the codex mock agent.
            assert "codex" in report["program"].lower()
            # Codex uses `exec` subcommand + positional prompt (no `-p` flag).
            assert "exec" in report["args"]
            assert "-p" not in report["args"]
            assert "--dangerously-bypass-approvals-and-sandbox" in report["args"]
            # The prompt appears as a positional arg somewhere in the args.
            assert any("do stuff" in a for a in report["args"])

    def test_codex_loop_dry_run(self, clud_binary: Path, mock_env: dict[str, str]) -> None:
        """`clud --codex --dry-run loop ...` emits the codex command."""
        result = _run(
            clud_binary,
            "--codex",
            "--dry-run",
            "loop",
            "task",
            env=mock_env,
        )
        assert result.returncode == 0, f"stderr: {result.stderr}"
        report = json.loads(result.stdout)
        assert report["backend"] == "codex"
        assert report["launch_mode"] == "subprocess"
        assert report["loop_markers"] is not None
        assert report["loop_markers"]["done_path"].replace("\\", "/").endswith(".clud/loop/DONE")
        assert report["iterations"] == 50
        cmd = report["command"]
        assert Path(cmd[0]).stem == "codex"
        assert "-c" in cmd
        assert 'plugins."github@openai-curated".enabled=false' in cmd
        assert cmd.index("exec") > 0
        assert "--dangerously-bypass-approvals-and-sandbox" in cmd
        assert "-p" not in cmd

    @pytest.mark.skipif(sys.platform != "win32", reason="Windows-only codex.cmd repro")
    def test_codex_loop_with_cmd_wrapper(
        self, clud_binary: Path, mock_env_codex_cmd: dict[str, str]
    ) -> None:
        result = _run(
            clud_binary,
            "--codex",
            "loop",
            "--loop-count",
            "2",
            "--no-done",
            "batch wrapper loop",
            env=mock_env_codex_cmd,
        )
        assert result.returncode == 0, f"stderr: {result.stderr}"
        cleaned = _strip_ansi(result.stdout)
        json_lines = [line.strip() for line in cleaned.splitlines() if line.strip().startswith("{")]
        assert len(json_lines) == 2
        for line in json_lines:
            report = json.loads(line)
            assert "exec" in report["args"]
            assert "--dangerously-bypass-approvals-and-sandbox" in report["args"]
            assert any("batch wrapper loop" in arg for arg in report["args"])


class TestInterruptReporting:
    """Verify Ctrl+C reports how clud was launched."""

    def test_timeout_diagnostics_describe_tree_and_tails(self, tmp_path: Path) -> None:
        log = tmp_path / "log.txt"
        log.write_text("x" * 5000 + "end", encoding="utf-8")
        report = describe_process_tree(os.getpid(), [os.getpid(), 2**22 + 1])
        assert f"client pid {os.getpid()}" in report
        assert "watched pids still alive" in report
        assert tail_file(log).endswith("end")
        assert len(tail_file(log)) == 2000
        assert tail_text(b"abc") == "abc"
        assert tail_file(tmp_path / "missing").startswith("<unreadable")

    @pytest.mark.skipif(sys.platform != "win32", reason="Windows-only cmd.exe prompt repro")
    @pytest.mark.parametrize(
        ("backend_args", "backend_name"),
        [
            ([], "claude"),
            (["--codex"], "codex"),
        ],
    )
    def test_cmd_wrapper_ctrl_break_exit_does_not_prompt_or_orphan_children(
        self,
        clud_binary: Path,
        mock_env_cmd_wrappers: dict[str, str],
        tmp_path: Path,
        backend_args: list[str],
        backend_name: str,
    ) -> None:
        pid_log = Path(mock_env_cmd_wrappers["RUNNING_PROCESS_CHILD_PID_LOG_PATH"])
        tree_log = tmp_path / f"{backend_name}-tree.jsonl"
        proc = process.Popen(
            [
                str(clud_binary),
                "--no-daemon",
                *backend_args,
                "-p",
                f"{backend_name} clean interrupt",
                "--",
                "--mock-sleep-ms",
                "15000",
                "--mock-spawn-tree-log",
                str(tree_log),
            ],
            stdin=process.PIPE,
            stdout=process.PIPE,
            stderr=process.PIPE,
            text=True,
            env=mock_env_cmd_wrappers,
            creationflags=process.CREATE_NEW_PROCESS_GROUP,
        )

        child_pids: list[int] = []
        tree_pids: list[int] = []
        try:
            child_pids = _wait_for_pid_log(pid_log, minimum=1)
            tree_pids = wait_for_tree_pids(tree_log, minimum=3)

            proc.send_signal(signal.CTRL_BREAK_EVENT)
            time.sleep(0.1)
            if proc.poll() is None:
                proc.send_signal(signal.CTRL_BREAK_EVENT)
            # 30s, not 10. This budget bounds *teardown* -- clud receiving the
            # break, reaping a three-process tree, and closing its pipes. The
            # Windows integration lane timed out here at 10s on a run that
            # changed only an unrelated unit-test file, and `main` fails this
            # lane in two of its last three runs, so 10 was measuring the
            # runner rather than the code (#994).
            #
            # Raising it past the mock's own 15s sleep is safe *because of the
            # `returncode == 130` assertion below*, not by accident: if the
            # break were dropped, the mock would run to completion and clud
            # would exit with its code, not 128+SIGINT, and the test would
            # still fail. Do not delete that assertion and leave this budget
            # alone -- together they are what makes a dropped break loud.
            #
            # 30 also has to fit. The waits here are sequential, and
            # pytest-timeout caps each test at 90s (pyproject.toml); exceeding
            # that trades a diagnosable TimeoutExpired for the timeout killer.
            try:
                stdout, stderr = proc.communicate(timeout=30)
            except process.TimeoutExpired as exc:
                # Snapshot before the finally block kills the tree (#2272).
                raise AssertionError(
                    f"{backend_name} client {proc.pid} did not exit 30s after Ctrl-Break\n"
                    f"{describe_process_tree(proc.pid or 0, child_pids + tree_pids)}\n"
                    f"--- client stdout tail ---\n{tail_text(exc.output)}\n"
                    f"--- client stderr tail ---\n{tail_text(exc.stderr)}\n"
                    f"--- mock child pid log ---\n{tail_file(pid_log)}\n"
                    f"--- mock tree log ---\n{tail_file(tree_log)}"
                ) from exc
        finally:
            if proc.poll() is None:
                kill_process(proc.pid)
                proc.wait(timeout=5)

        combined = f"{stdout}\n{stderr}"
        # 15, not the 30 above: by the time communicate() has returned, clud
        # has already exited and these children have already been signalled, so
        # this waits for handles to close rather than for reaping to run.
        # It also has to fit -- 10 + 10 + 30 + 30 = 80 against a 90s ceiling is
        # not margin. This way the worst case is 65s.
        wait_for_pids_to_exit(child_pids + tree_pids, timeout=15)
        assert proc.returncode == 130, combined
        assert "Terminate batch job" not in combined

    def test_ctrl_c_reports_pty_mode_when_forced(
        self, clud_binary: Path, mock_env: dict[str, str]
    ) -> None:
        kwargs: dict[str, Any] = {}
        if sys.platform == "win32":
            kwargs["creationflags"] = process.CREATE_NEW_PROCESS_GROUP

        # PR #145 gated the "[clud] interrupted via Ctrl+C (pty)" diagnostic
        # behind `--verbose`; pass that flag here so the assertion below has
        # the message to check against. Without `--verbose` the diagnostic is
        # silenced by design and the test had been failing on every platform's
        # integration run since #145 landed.
        # PR #251 added a second variant on the daemon-handoff path
        # ("(pty, handed to daemon)"). The assertion below matches the common
        # prefix "(pty" so it accepts both forms.
        proc = process.Popen(
            [
                str(clud_binary),
                "--verbose",
                "--pty",
                "-p",
                "hello",
                "--",
                "--mock-sleep-ms",
                "5000",
            ],
            stdout=process.PIPE,
            stderr=process.PIPE,
            text=True,
            env=mock_env,
            **kwargs,
        )

        try:
            time.sleep(0.5)
            if sys.platform == "win32":
                proc.send_signal(signal.CTRL_BREAK_EVENT)
            else:
                proc.send_signal(signal.SIGINT)
            _stdout, stderr = proc.communicate(timeout=10)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=5)

        if sys.platform == "win32":
            assert is_ctrl_c_exit(proc.returncode), proc.returncode
        else:
            assert proc.returncode == 130

        if proc.returncode == 130:
            assert "[clud] interrupted via Ctrl+C (pty" in stderr


class TestStdinForwarding:
    """Verify stdin data reaches the backend in subprocess mode.

    When clud is launched with an explicit prompt (-p), the pipe-mode stdin
    detection is skipped.  Any data written to clud's stdin should be
    inherited by the child process (subprocess mode uses StdinMode::Inherit).

    The mock agent's --mock-read-stdin-ms flag makes it read from stdin for
    a specified duration and report what it received.
    """

    def _parse_report(
        self,
        report_file: Path,
        stdout: str,
        stderr: str,
    ) -> dict:
        """Parse the mock agent report from file or stdout."""
        if report_file.exists():
            return json.loads(report_file.read_text())
        return _parse_agent_report(
            process.CompletedProcess(args=[], returncode=0, stdout=stdout, stderr=stderr)
        )

    def test_subprocess_stdin_forwarding(
        self, clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
    ) -> None:
        """Data written to clud's stdin reaches the child in subprocess mode."""
        report_file = tmp_path / "report.json"
        result = _run(
            clud_binary,
            "-p",
            "hello",
            "--",
            "--mock-read-stdin-ms",
            "3000",
            "--mock-report-file",
            str(report_file),
            env=mock_env,
            input_data="dropped-file.txt\n",
        )
        assert result.returncode == 0, f"stderr: {result.stderr}"

        report = self._parse_report(report_file, result.stdout, result.stderr)
        assert report["stdin"] is not None, "mock agent received no stdin data"
        assert "dropped-file.txt" in report["stdin"]

    def test_subprocess_stdin_is_not_terminal_when_piped(
        self, clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
    ) -> None:
        """When test pipes stdin, the child sees a pipe (not a terminal)."""
        report_file = tmp_path / "report.json"
        result = _run(
            clud_binary,
            "-p",
            "hello",
            "--",
            "--mock-read-stdin-ms",
            "1000",
            "--mock-report-file",
            str(report_file),
            env=mock_env,
            input_data="",
        )
        assert result.returncode == 0, f"stderr: {result.stderr}"

        report = self._parse_report(report_file, result.stdout, result.stderr)
        # With piped stdin, the child should NOT see a terminal
        assert report["stdin_is_terminal"] is False

    def test_subprocess_multiline_stdin(
        self, clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
    ) -> None:
        """Multi-line paste (simulated drag-and-drop of multiple files) works."""
        report_file = tmp_path / "report.json"
        payload = "file1.txt\nfile2.txt\npath/to/file3.txt\n"

        result = _run(
            clud_binary,
            "-p",
            "hello",
            "--",
            "--mock-read-stdin-ms",
            "3000",
            "--mock-report-file",
            str(report_file),
            env=mock_env,
            input_data=payload,
        )
        assert result.returncode == 0, f"stderr: {result.stderr}"

        report = self._parse_report(report_file, result.stdout, result.stderr)
        assert report["stdin"] is not None
        assert "file1.txt" in report["stdin"]
        assert "file2.txt" in report["stdin"]
        assert "path/to/file3.txt" in report["stdin"]

    def test_subprocess_bracketed_paste_forwarding(
        self, clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
    ) -> None:
        """Bracketed paste sequences are forwarded to the child."""
        report_file = tmp_path / "report.json"
        # Simulate bracketed paste: ESC[200~ ... ESC[201~
        pasted = "\x1b[200~/path/to/dropped.txt\x1b[201~"

        result = _run(
            clud_binary,
            "-p",
            "hello",
            "--",
            "--mock-read-stdin-ms",
            "3000",
            "--mock-report-file",
            str(report_file),
            env=mock_env,
            input_data=pasted,
        )
        assert result.returncode == 0, f"stderr: {result.stderr}"

        report = self._parse_report(report_file, result.stdout, result.stderr)
        assert report["stdin"] is not None
        assert "/path/to/dropped.txt" in report["stdin"]


class TestLargeFileWarningStartup:
    """Regression for zackees/clud#515: the large-source-file warning must
    list every offending file (path + size) on a *normal* backend launch,
    not just the header. The bug wiped the detail lines during Codex PTY
    startup while `--dry-run` (which never starts a backend) showed them.
    """

    def _repo_with_big_file(self, tmp_path: Path) -> tuple[Path, str]:
        repo = tmp_path / "repo"
        repo.mkdir()
        (repo / ".git").mkdir()
        name = "huge.rs"
        # Comfortably over the 40 kB / ~1000 LOC guard threshold.
        (repo / name).write_text("x" * (60 * 1024))
        return repo, name

    def test_entries_present_on_normal_codex_startup(
        self, clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
    ) -> None:
        repo, name = self._repo_with_big_file(tmp_path)
        result = _run(clud_binary, "--codex", "-p", "hello", env=mock_env, cwd=repo)
        assert result.returncode == 0, f"stderr: {result.stderr}"
        assert "large source files" in result.stderr, (
            f"warning header missing on normal startup:\n{result.stderr}"
        )
        # The regression: the file entry (not only the header) must be present.
        assert name in result.stderr, (
            f"file entry '{name}' missing from normal-startup warning:\n{result.stderr}"
        )

    def test_normal_startup_matches_dry_run_entries(
        self, clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
    ) -> None:
        repo, name = self._repo_with_big_file(tmp_path)
        normal = _run(clud_binary, "--codex", "-p", "hello", env=mock_env, cwd=repo)
        dry = _run(clud_binary, "--codex", "--dry-run", "-p", "hello", env=mock_env, cwd=repo)
        # Both paths surface the same file entry — parity is the acceptance
        # criterion: normal startup must not silently drop what dry-run shows.
        assert name in dry.stderr, f"dry-run missing entry:\n{dry.stderr}"
        assert name in normal.stderr, f"normal startup missing entry:\n{normal.stderr}"


@pytest.mark.parametrize("backend", ["claude", "codex"])
@pytest.mark.parametrize("bias", ["merge", "pr", "off"])
def test_land_bias_reaches_mock_agent(
    clud_binary: Path, mock_env: dict[str, str], backend: str, bias: str
) -> None:
    mock_env["CLUD_LAND_BIAS"] = bias
    result = _run(clud_binary, f"--{backend}", "--safe", "-p", "hello", env=mock_env)
    assert result.returncode == 0, result.stderr
    report = _parse_agent_report(result)
    argv = " ".join(report["args"])
    assert ("Land the work" in argv) == (bias != "off")
    if bias == "pr":
        assert "Do not merge unless" in argv
    if bias == "merge":
        assert "merged PR" in argv
    assert "--dangerously-skip-permissions" not in report["args"]
    assert "--dangerously-bypass-approvals-and-sandbox" not in report["args"]

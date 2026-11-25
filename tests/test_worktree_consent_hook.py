"""End-to-end coverage for the worktree consent control (#1984).

The decision table and the prompt phrasings are asserted in
`worktree_consent.rs` and `daemon/session_flags.rs`. These tests run the real
`clud-cmd-scan` binary against a mock clud daemon, to check what only a real
process shows: the `UserPromptSubmit` hook records consent (or not) in the
daemon, and a PreToolUse payload that creates a worktree comes back as a
hook-protocol denial when the session never allowed it.
"""

from __future__ import annotations

import json
import os
import shutil
import socketserver
import sys
import threading
from collections.abc import Iterator
from pathlib import Path

import pytest

from tests import process

SESSION = "sess-1984"
DENIED = "Git worktrees are off for this session"


def _binary_name(name: str) -> str:
    return f"{name}.exe" if sys.platform == "win32" else name


def _cmd_scan_binary() -> Path:
    clud_binary = os.environ.get("CLUD_TEST_BINARY")
    if clud_binary:
        sibling = Path(clud_binary).with_name(_binary_name("clud-cmd-scan"))
        if sibling.is_file():
            return sibling
    resolved = shutil.which(_binary_name("clud-cmd-scan"))
    if resolved:
        return Path(resolved)
    raise AssertionError("clud-cmd-scan test binary not found")


class MockDaemon:
    """The daemon's session-flag lane: one JSON line in, one line out."""

    def __init__(self) -> None:
        self.flags: dict[str, dict[str, object]] = {}
        self.requests: list[dict[str, object]] = []
        daemon = self

        class Handler(socketserver.StreamRequestHandler):
            def handle(self) -> None:
                line = self.rfile.readline()
                if not line:
                    return
                request = json.loads(line)
                daemon.requests.append(request)
                reply = daemon.answer(request)
                self.wfile.write((json.dumps(reply) + "\n").encode())

        self.server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def answer(self, request: dict[str, object]) -> dict[str, object]:
        op = request.get("session_flag_op")
        session = str(request.get("session_id"))
        key = str(request.get("key"))
        if op == "set":
            self.flags.setdefault(session, {})[key] = request.get("value")
            return {"session_flag_op": "set"}
        if op == "get":
            return {"session_flag_op": "get", "value": self.flags.get(session, {}).get(key)}
        return {"checkout_op": "who", "occupants": []}

    def sets(self) -> list[dict[str, object]]:
        return [r for r in self.requests if r.get("session_flag_op") == "set"]

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


@pytest.fixture
def world(tmp_path: Path) -> Iterator[dict[str, object]]:
    home = tmp_path / "home"
    state = tmp_path / "state"
    home.mkdir()
    state.mkdir()
    repo = tmp_path / "repo"
    repo.mkdir()
    process.run(["git", "init", "-q", str(repo)], check=True, capture_output=True, timeout=30)
    daemon = MockDaemon()
    (state / "daemon.json").write_text(
        json.dumps({"pid": os.getpid(), "port": daemon.port}), encoding="utf-8"
    )
    env = os.environ.copy()
    env["HOME"] = str(home)
    env["USERPROFILE"] = str(home)
    env["CLUD_DAEMON_STATE_DIR"] = str(state)
    # A clud launch that registers the prompt hook sets this marker.
    env["CLUD_WORKTREE_GATE"] = "1"
    for key in (
        "CLUD_ALLOW_WORKTREES",
        "CLUD_UNSAFE_MODE",
        "CLUD_ALLOW_ALL_CMDS",
        "CLUD_BAD_CMD_OVERRIDE",
        "CLUD_HOOK_DISPATCH",
    ):
        env.pop(key, None)
    yield {"home": home, "state": state, "repo": repo, "daemon": daemon, "env": env}
    daemon.close()


def _hook(
    world: dict[str, object], payload: dict[str, object], *, event: str | None = None
) -> process.CompletedProcess[str]:
    args = [str(_cmd_scan_binary())]
    if event:
        args += ["--event", event]
    return process.run(
        args,
        input=json.dumps(payload),
        capture_output=True,
        text=True,
        env=world["env"],
        timeout=30,
    )


def _prompt(
    world: dict[str, object], text: str, session: str = SESSION
) -> process.CompletedProcess[str]:
    return _hook(
        world,
        {
            "session_id": session,
            "prompt": text,
            "cwd": str(world["repo"]),
            "hook_event_name": "UserPromptSubmit",
        },
        event="UserPromptSubmit",
    )


def _worktree_add(
    world: dict[str, object], session: str = SESSION
) -> process.CompletedProcess[str]:
    home = Path(str(world["home"]))
    dest = home / ".clud" / "worktrees" / "repo-wt-1984"
    return _hook(
        world,
        {
            "session_id": session,
            "tool_name": "Bash",
            "cwd": str(world["repo"]),
            "tool_input": {"command": f"git worktree add -b feat/x {dest.as_posix()}"},
        },
    )


def _tool(
    world: dict[str, object], tool: str, tool_input: dict[str, object], session: str = SESSION
) -> process.CompletedProcess[str]:
    return _hook(
        world,
        {
            "session_id": session,
            "tool_name": tool,
            "cwd": str(world["repo"]),
            "tool_input": tool_input,
        },
    )


def _assert_denied(result: process.CompletedProcess[str], why: str) -> None:
    assert result.returncode == 2, result.stdout + result.stderr
    assert '"deny"' in result.stdout, result.stdout
    assert DENIED in result.stdout, result.stdout
    assert why in result.stdout, result.stdout


def test_no_mention_of_worktrees_means_every_creation_path_is_denied(
    world: dict[str, object],
) -> None:
    result = _prompt(world, "fix the login bug and open a PR")
    assert result.returncode == 0, result.stderr
    assert result.stdout == "", "the prompt hook must print nothing"
    assert world["daemon"].sets() == [], "no mention must not record consent"

    _assert_denied(_worktree_add(world), "has not allowed")
    _assert_denied(_tool(world, "EnterWorktree", {}), "has not allowed")
    _assert_denied(
        _tool(world, "Agent", {"description": "x", "prompt": "x", "isolation": "worktree"}),
        "has not allowed",
    )


def test_a_mention_that_says_no_is_still_denied(world: dict[str, object]) -> None:
    for text in (
        "Just do one agent - no worktrees",
        "Don't use the git work tree. Just do them one at a time in this repo",
        "worktrees not allowed",
        "worktrees aren't allowed",
        "find stale worktrees that we can purge",
    ):
        assert _prompt(world, text).returncode == 0
    assert world["daemon"].sets() == [
        {"session_flag_op": "set", "session_id": SESSION, "key": "worktrees", "value": False}
    ] * 4
    _assert_denied(_worktree_add(world), "has not allowed")


def test_a_yes_allows_that_session_only(world: dict[str, object]) -> None:
    assert _prompt(world, "you can use worktrees for this").returncode == 0
    assert world["daemon"].sets() == [
        {"session_flag_op": "set", "session_id": SESSION, "key": "worktrees", "value": True}
    ]

    allowed = _tool(world, "EnterWorktree", {})
    assert allowed.returncode == 0, allowed.stdout + allowed.stderr
    _assert_denied(_tool(world, "EnterWorktree", {}, session="other-session"), "has not allowed")


def test_a_worktree_direction_permits_agent_isolation(world: dict[str, object]) -> None:
    tool_input = {"description": "x", "prompt": "x", "isolation": "worktree"}
    assert _prompt(world, "We could do this in a worktree").returncode == 0
    _assert_denied(_tool(world, "Agent", tool_input), "has not allowed")
    assert _prompt(world, "spawning a sub agent to make this in a worktree").returncode == 0
    allowed = _tool(world, "Agent", tool_input)
    assert allowed.returncode == 0, allowed.stdout + allowed.stderr


def test_a_later_no_revokes_previously_granted_consent(world: dict[str, object]) -> None:
    assert _prompt(world, "use worktrees").returncode == 0
    assert _tool(world, "EnterWorktree", {}).returncode == 0
    assert _prompt(world, "worktrees are not allowed now").returncode == 0
    _assert_denied(_tool(world, "EnterWorktree", {}), "has not allowed")


def test_an_agent_isolation_without_worktree_is_untouched(world: dict[str, object]) -> None:
    result = _tool(world, "Agent", {"description": "x", "prompt": "x"})
    assert result.returncode == 0, result.stdout + result.stderr


def test_without_a_daemon_the_session_is_denied(world: dict[str, object]) -> None:
    (Path(str(world["state"])) / "daemon.json").unlink()
    _assert_denied(_tool(world, "EnterWorktree", {}), "could not be asked")


def test_the_launch_environment_allows_and_a_command_prefix_does_not(
    world: dict[str, object],
) -> None:
    dest = Path(str(world["home"])) / ".clud" / "worktrees" / "repo-wt-y"
    prefixed = _hook(
        world,
        {
            "session_id": SESSION,
            "tool_name": "Bash",
            "cwd": str(world["repo"]),
            "tool_input": {
                "command": f"CLUD_ALLOW_WORKTREES=1 git worktree add -b feat/y {dest.as_posix()}"
            },
        },
    )
    _assert_denied(prefixed, "has not allowed")

    world["env"]["CLUD_ALLOW_WORKTREES"] = "1"  # type: ignore[index]
    allowed = _tool(world, "EnterWorktree", {})
    assert allowed.returncode == 0, allowed.stdout + allowed.stderr


def test_the_worktree_create_backstop_refuses_without_consent(world: dict[str, object]) -> None:
    # Claude Code builds every worktree of its own (EnterWorktree, isolated
    # agents, --worktree) through this event; exit 1 fails the creation.
    payload = {"session_id": SESSION, "cwd": str(world["repo"]), "name": "agent-x"}
    result = _hook(world, payload, event="WorktreeCreate")
    assert result.returncode == 1, result.stdout + result.stderr
    assert DENIED in result.stderr, result.stderr
    assert result.stdout == "", "nothing may reach stdout, which Claude Code reads as the path"


def test_a_session_clud_did_not_gate_keeps_its_worktrees(world: dict[str, object]) -> None:
    # A plain `claude` or Codex session running clud-cmd-scan from a committed
    # or global hook config has no prompt hook, so it can never consent.
    world["env"].pop("CLUD_WORKTREE_GATE")  # type: ignore[union-attr]
    (Path(str(world["state"])) / "daemon.json").unlink()
    for result in (
        _tool(world, "EnterWorktree", {}),
        _tool(world, "Agent", {"description": "x", "prompt": "x", "isolation": "worktree"}),
    ):
        assert result.returncode == 0, result.stdout + result.stderr
        assert DENIED not in result.stdout

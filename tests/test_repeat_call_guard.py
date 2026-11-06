"""End-to-end coverage for the repeated-identical-call guard (#1674).

The Rust decision table is asserted in `block_bad_cmd_repeat_tests.rs`; these
tests drive the real `clud-cmd-scan` process with the #1276 incident shape
(294 identical Bash PreToolUse payloads in one session) under a temp HOME and
state dir, so the developer's real `~/.claude` and `~/.clud` are never touched.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
from pathlib import Path

from tests import process

DEFAULT_LIMIT = 200


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


def _env(tmp_path: Path, **extra: str) -> dict[str, str]:
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    env = os.environ.copy()
    for key in ("CLUD_REPEAT_CALL_LIMIT", "CLUD_ALLOW_ALL_CMDS", "CLUD_BAD_CMD_OVERRIDE"):
        env.pop(key, None)
    env.update(
        HOME=str(home),
        USERPROFILE=str(home),
        CLUD_DAEMON_STATE_DIR=str(tmp_path / "state"),
        CLUD_SKIP_RM_IDENTITY="1",
    )
    env.update(extra)
    return env


def _call(tmp_path: Path, env: dict[str, str], command: str, session: str = "s-1") -> int:
    payload = json.dumps(
        {
            "session_id": session,
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "cwd": str(tmp_path),
            "tool_input": {"command": command},
        }
    )
    result = process.run(
        [str(_cmd_scan_binary())],
        input=payload,
        cwd=tmp_path,
        capture_output=True,
        text=True,
        env=env,
        timeout=30,
    )
    if result.returncode == 2:
        assert "repeat-guard" in result.stdout, result.stdout + result.stderr
    return result.returncode


def test_incident_294_identical_calls_are_denied_from_call_n_plus_one(tmp_path: Path) -> None:
    env = _env(tmp_path)
    assert _call(tmp_path, env, "echo ready") == 0
    state = tmp_path / "state" / "repeat-guard"
    files = list(state.iterdir())
    assert len(files) == 1
    # One real call wrote the streak; jump to just below the default limit so
    # the compiled-in boundary is proved without 294 process spawns.
    streak = json.loads(files[0].read_text(encoding="utf-8"))
    assert streak["count"] == 1
    streak["count"] = DEFAULT_LIMIT - 2
    files[0].write_text(json.dumps(streak), encoding="utf-8")
    codes = [_call(tmp_path, env, "echo ready") for _ in range(4)]
    # Calls DEFAULT_LIMIT-1 and DEFAULT_LIMIT pass; the next two are denied.
    assert codes == [0, 0, 2, 2]
    for file in files:
        text = file.read_text(encoding="utf-8")
        assert "echo" not in text, "state stores only hashes and counts"
        assert "s-1" not in text, "state never stores the session id"


def test_differing_calls_and_sessions_reset_the_streak(tmp_path: Path) -> None:
    env = _env(tmp_path, CLUD_REPEAT_CALL_LIMIT="3")
    assert [_call(tmp_path, env, "gh pr checks") for _ in range(3)] == [0, 0, 0]
    assert _call(tmp_path, env, "sleep 30") == 0
    assert [_call(tmp_path, env, "gh pr checks") for _ in range(3)] == [0, 0, 0]
    assert _call(tmp_path, env, "gh pr checks", session="s-2") == 0
    assert _call(tmp_path, env, "gh pr checks") == 2


def test_override_is_audited_and_limit_zero_disables(tmp_path: Path) -> None:
    env = _env(tmp_path, CLUD_REPEAT_CALL_LIMIT="2")
    command = "CLUD_ALLOW_REPEAT=1 gh pr checks"
    assert [_call(tmp_path, env, command) for _ in range(5)] == [0] * 5
    log = tmp_path / "home" / ".clud" / "tools" / "hooks" / "block-bad-cmd.log"
    assert "REPEAT-GUARD-OVERRIDE" in log.read_text(encoding="utf-8", errors="replace")

    off = _env(tmp_path, CLUD_REPEAT_CALL_LIMIT="0")
    assert [_call(tmp_path, off, "echo x") for _ in range(5)] == [0] * 5


def test_unwritable_state_fails_open(tmp_path: Path) -> None:
    blocker = tmp_path / "blocker"
    blocker.write_text("not a dir", encoding="utf-8")
    env = _env(tmp_path, CLUD_REPEAT_CALL_LIMIT="1", CLUD_DAEMON_STATE_DIR=str(blocker))
    assert [_call(tmp_path, env, "echo x") for _ in range(4)] == [0] * 4

"""The patient-poll advisory note, driven through the real command scan (#2631).

The repeat guard resets after a 120 s gap, so one query repeated for hours
never trips it. These tests execute `clud __cmd-scan` exactly as the hook
launcher does and assert the notice appears at the two counts and nowhere else.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Any

import pytest

REPO = Path(__file__).resolve().parents[1]


def scanner(tmp_path: Path) -> str:
    candidate = os.environ.get("CLUD_TEST_BINARY")
    if candidate and Path(candidate).is_file():
        return str(Path(candidate).resolve())
    built = REPO / "target" / "debug" / "clud"
    if built.is_file():
        return str(built)
    pytest.skip("build clud (cargo build -p clud --bins) or set CLUD_TEST_BINARY")


def clean_env(state_dir: Path) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if not key.startswith("CLUD")}
    env["CLUD_DAEMON_STATE_DIR"] = str(state_dir)
    return env


def scan(binary: str, state_dir: Path, session: str, command: str) -> dict[str, Any]:
    payload = {
        "hook_event_name": "PreToolUse",
        "session_id": session,
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": str(REPO),
        "transcript_path": str(state_dir / "transcript.jsonl"),
    }
    proc = subprocess.run(
        [binary, "__cmd-scan"],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
        timeout=120,
        env=clean_env(state_dir),
        check=False,
    )
    assert proc.returncode == 0, proc.stderr
    return json.loads(proc.stdout) if proc.stdout.strip() else {}


def context(output: dict[str, Any]) -> str:
    return (output.get("hookSpecificOutput") or {}).get("additionalContext") or ""


def test_the_poll_note_appears_at_six_and_twenty_four_only(tmp_path: Path) -> None:
    binary = scanner(tmp_path)
    state = tmp_path / "state"
    state.mkdir()
    noted = [
        index
        for index in range(1, 27)
        if "poll-note" in context(scan(binary, state, "s1", "gh issue view 42 --repo o/r"))
    ]
    assert noted == [6, 24]


def test_a_build_command_never_gets_the_poll_note(tmp_path: Path) -> None:
    binary = scanner(tmp_path)
    state = tmp_path / "state"
    state.mkdir()
    for index in range(1, 9):
        output = scan(binary, state, "s2", "soldr cargo test -p clud")
        assert "poll-note" not in context(output), index


def test_a_write_command_never_gets_the_poll_note(tmp_path: Path) -> None:
    binary = scanner(tmp_path)
    state = tmp_path / "state"
    state.mkdir()
    for index in range(1, 9):
        output = scan(binary, state, "s3", "gh pr create --fill")
        assert "poll-note" not in context(output), index


def test_the_note_never_denies(tmp_path: Path) -> None:
    binary = scanner(tmp_path)
    state = tmp_path / "state"
    state.mkdir()
    for _ in range(6):
        output = scan(binary, state, "s4", "git status --short")
    assert (output.get("hookSpecificOutput") or {}).get("permissionDecision") is None

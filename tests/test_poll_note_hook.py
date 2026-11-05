"""The patient-poll advisory note, driven through the real command scan (#2631).

The repeat guard resets after a 120 s gap, so one query repeated for hours
never trips it. These tests execute `clud __cmd-scan` exactly as the hook
launcher does and assert the notice appears at the two counts and nowhere else.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

import pytest
from running_process import PIPE, RunningProcess

REPO = Path(__file__).resolve().parents[1]


def scanner(tmp_path: Path) -> str:
    candidate = os.environ.get("CLUD_TEST_BINARY")
    if candidate and Path(candidate).is_file():
        return str(Path(candidate).resolve())
    built = REPO / "target" / "debug" / "clud"
    if built.is_file():
        return str(built)
    pytest.skip("build clud (cargo build -p clud --bins) or set CLUD_TEST_BINARY")


def clean_env(state_dir: Path, extra: dict[str, str] | None = None) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if not key.startswith("CLUD")}
    env["CLUD_DAEMON_STATE_DIR"] = str(state_dir)
    env.update(extra or {})
    return env


def scan(
    binary: str,
    state_dir: Path,
    session: str,
    command: str,
    extra: dict[str, str] | None = None,
) -> dict[str, Any]:
    payload = {
        "hook_event_name": "PreToolUse",
        "session_id": session,
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": str(REPO),
        "transcript_path": str(state_dir / "transcript.jsonl"),
    }
    # stderr=PIPE: running-process merges stderr into stdout otherwise (#1175).
    proc = RunningProcess.run(
        [binary, "__cmd-scan"],
        input=json.dumps(payload),
        capture_output=True,
        stderr=PIPE,
        text=True,
        timeout=120,
        env=clean_env(state_dir, extra),
    )
    # A refusal is exit 2 with the same JSON body on stdout; anything else is a
    # harness fault, not a verdict.
    assert proc.returncode in (0, 2), proc.stderr
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


# ---- #2631 follow-up: the notes are advisory, the third step stops ----------

# The exact shape measured on 2026-11-05: one session re-issued this query 127
# times in 24 h, and 618 times across five days, waiting on an upstream repo.
OBS_POLL = (
    "for n in 3183 3184; do gh issue view $n --repo obs-rust/obs-studio --json state,comments; done"
)


def decision(output: dict[str, Any]) -> str | None:
    return (output.get("hookSpecificOutput") or {}).get("permissionDecision")


def test_the_patient_poll_is_refused_past_its_limit_through_the_real_hook(
    tmp_path: Path,
) -> None:
    binary = scanner(tmp_path)
    state = tmp_path / "state"
    state.mkdir()
    denied: list[int] = []
    noted: list[int] = []
    for index in range(1, 99):
        output = scan(binary, state, "poll", OBS_POLL)
        if "poll-note" in context(output):
            noted.append(index)
        if decision(output) == "deny":
            denied.append(index)
    assert noted == [6, 24]
    assert denied == [97, 98]


def test_a_build_command_is_never_poll_refused(tmp_path: Path) -> None:
    binary = scanner(tmp_path)
    state = tmp_path / "state"
    state.mkdir()
    for index in range(1, 100):
        output = scan(binary, state, "build", "soldr cargo test -p clud")
        assert decision(output) is None, index


def test_a_different_call_resets_the_poll_count(tmp_path: Path) -> None:
    binary = scanner(tmp_path)
    state = tmp_path / "state"
    state.mkdir()
    for _ in range(96):
        scan(binary, state, "reset", OBS_POLL)
    scan(binary, state, "reset", "git ls-remote origin")
    output = scan(binary, state, "reset", OBS_POLL)
    assert decision(output) is None


def test_the_poll_limit_env_moves_the_stop(tmp_path: Path) -> None:
    binary = scanner(tmp_path)
    state = tmp_path / "state"
    state.mkdir()
    denied: list[int] = []
    for index in range(1, 8):
        output = scan(binary, state, "env", OBS_POLL, extra={"CLUD_POLL_CALL_LIMIT": "6"})
        if decision(output) == "deny":
            denied.append(index)
    assert denied == [7]
    reason = (output.get("hookSpecificOutput") or {}).get("permissionDecisionReason") or ""
    assert "wait/until.py" in reason
    assert "CLUD_ALLOW_REPEAT=1" in reason

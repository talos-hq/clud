"""End-to-end hook delivery through clud's compiled matcher (#1954).

A harness filters PreToolUse hooks by matcher before any hook runs, so a rule
for a tool the matcher misses is dead code. Until #1954 clud registered
`Bash` alone, and this repo's own catch-all matcher hid that from everyone
developing clud. These tests use a repo with no hook config of its own: the
only matchers are the ones `clud doctor --json` reports for the launch
fragment. A mock harness applies them the way Claude Code does (anchored
regex over the tool name) and pipes each delivered call to the real
`clud-cmd-scan`.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import sys
from pathlib import Path
from typing import Any

from tests import process


def _binary_name(name: str) -> str:
    return f"{name}.exe" if sys.platform == "win32" else name


def _binary(name: str) -> Path:
    clud_binary = os.environ.get("CLUD_TEST_BINARY")
    if clud_binary:
        sibling = Path(clud_binary).with_name(_binary_name(name))
        if sibling.is_file():
            return sibling
    resolved = shutil.which(_binary_name(name))
    if resolved:
        return Path(resolved)
    raise AssertionError(f"{name} test binary not found")


def _env(tmp_path: Path, **extra: str) -> dict[str, str]:
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    env = os.environ.copy()
    env["HOME"] = str(home)
    env["USERPROFILE"] = str(home)
    for key in ("CLUD_BAD_CMD_OVERRIDE", "CLUD_ALLOW_GRIND_AGENTS"):
        env.pop(key, None)
    env.update(extra)
    return env


def _repo(tmp_path: Path) -> Path:
    repo = tmp_path / "repo"
    (repo / ".git").mkdir(parents=True)
    return repo


def _launch_matchers(tmp_path: Path, repo: Path) -> list[str]:
    result = process.run(
        [str(_binary("clud")), "doctor", "--json"],
        capture_output=True,
        text=True,
        env=_env(tmp_path),
        cwd=str(repo),
        timeout=60,
    )
    report = json.loads(result.stdout)
    sources = {entry["source"]: entry for entry in report["coverage"]}
    assert list(sources) == ["clud launch --settings fragment"], (
        "the repo and home carry no hook config, so only the launch fragment counts"
    )
    return sources["clud launch --settings fragment"]["matchers"]


def _delivered(matchers: list[str], tool: str) -> bool:
    return any(
        m in ("", "*") or re.fullmatch(f"(?:{m})", tool) is not None for m in matchers
    )


def _deliver(
    tmp_path: Path, repo: Path, matchers: list[str], payload: dict[str, Any], **env: str
) -> process.CompletedProcess[str] | None:
    """Run the scanner for `payload` iff the harness would deliver it."""
    if not _delivered(matchers, payload["tool_name"]):
        return None
    return process.run(
        [str(_binary("clud-cmd-scan"))],
        input=json.dumps({"cwd": str(repo), "session_id": "hook-1954", **payload}),
        capture_output=True,
        text=True,
        env=_env(tmp_path, **env),
        timeout=30,
    )


def _denied(result: process.CompletedProcess[str] | None) -> bool:
    assert result is not None, "the compiled matcher never delivered this call"
    return result.returncode == 2 or '"deny"' in result.stdout


def test_non_bash_rules_are_delivered_and_enforced_without_a_catch_all(
    tmp_path: Path,
) -> None:
    repo = _repo(tmp_path)
    matchers = _launch_matchers(tmp_path, repo)
    assert "*" not in matchers

    planner_write = {
        "tool_name": "Write",
        "agent_type": "grind-planner",
        "tool_input": {"file_path": str(repo / "plan.md"), "content": "x"},
    }
    assert _denied(_deliver(tmp_path, repo, matchers, planner_write))

    grind_spawn = {
        "tool_name": "Agent",
        "tool_input": {"subagent_type": "grind-lander", "prompt": "x"},
    }
    assert _denied(_deliver(tmp_path, repo, matchers, grind_spawn))

    # ScheduleWakeup reaches the dispatcher; its first-round stop guard
    # decides from the transcript, which the Rust tests cover.
    assert _delivered(matchers, "ScheduleWakeup")


def test_codex_tool_names_are_delivered_and_enforced(tmp_path: Path) -> None:
    repo = _repo(tmp_path)
    matchers = _launch_matchers(tmp_path, repo)

    planner_patch = {
        "tool_name": "apply_patch",
        "agent_type": "grind-planner",
        "tool_input": {"command": "*** Begin Patch\n*** Add File: plan.md\n+x\n*** End Patch\n"},
    }
    assert _denied(_deliver(tmp_path, repo, matchers, planner_patch))

    nested_spawn = {
        "tool_name": "spawn_agent",
        "agent_id": "agent-7",
        "tool_input": {"agent_type": "explorer", "message": "x"},
    }
    assert _denied(
        _deliver(tmp_path, repo, matchers, nested_spawn, CLUD_BLOCK_RECURSIVE_AGENTS="1")
    )


def test_unrelated_tools_never_reach_the_scanner(tmp_path: Path) -> None:
    repo = _repo(tmp_path)
    matchers = _launch_matchers(tmp_path, repo)
    for tool in ("Grep", "Glob", "WebFetch", "TodoWrite", "ToolSearch"):
        assert not _delivered(matchers, tool), tool


def test_read_is_delivered_for_the_unchanged_reread_guard(tmp_path: Path) -> None:
    """#2233: `Read` has a rule, so the matcher must route it to the scanner."""
    repo = _repo(tmp_path)
    matchers = _launch_matchers(tmp_path, repo)
    assert _delivered(matchers, "Read")

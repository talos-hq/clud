"""Loop-scoped plan-mode policy through the real hook and isolated daemon."""

from __future__ import annotations

import json
import os
import shutil
from collections.abc import Iterator
from pathlib import Path

import pytest

from tests import process
from tests.metered_consent import consent

SESSION = "loop-plan-mode-1844"
DENIED = "loop-plan-mode"


@pytest.fixture
def loop_world(tmp_path: Path) -> Iterator[tuple[str, dict[str, str], Path]]:
    binary = os.environ.get("CLUD_TEST_BINARY") or shutil.which("clud")
    assert binary, "build the clud test binary first"
    home = tmp_path / "home"
    home.mkdir()
    repo = tmp_path / "repo"
    repo.mkdir()
    (repo / ".git").mkdir()
    env = os.environ.copy()
    for name in tuple(env):
        if name.startswith("CLUD_"):
            env.pop(name)
    env.update(
        {
            "HOME": str(home),
            "USERPROFILE": str(home),
            "CLUD_HOOK_HOME": str(home),
            "CLUD_DAEMON_STATE_DIR": str(tmp_path / "state"),
            "CLUD_DAEMON_TEST_MODE": "1",
            "CLUD_INTEGRATION_TESTS": "1",
            "CLUD_NO_UNLOCK": "1",
            "CLUD_SERVER_SETTINGS": "0",
            "SOLDR_BROKER_AUTOSPAWN": "0",
        }
    )
    try:
        started = process.run(
            [binary, "daemon", "api-info", "--json"],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )
        assert started.returncode == 0, started.stdout + started.stderr
        yield binary, env, repo
    finally:
        process.run(
            [binary, "daemon", "stop"],
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )


def _hook(
    world: tuple[str, dict[str, str], Path], event: str, **fields: object
) -> process.CompletedProcess[str]:
    binary, env, repo = world
    return process.run(
        [binary, "__cmd-scan", "--event", event],
        input=json.dumps(
            {"session_id": SESSION, "cwd": str(repo), "hook_event_name": event, **fields}
        ),
        env=env,
        cwd=repo,
        capture_output=True,
        text=True,
        timeout=30,
    )


def test_loop_prompt_denies_plan_mode(loop_world: tuple[str, dict[str, str], Path]) -> None:
    prompt = _hook(loop_world, "UserPromptSubmit", prompt="/loop 20m fix the next issue")
    assert prompt.returncode == 0, prompt.stderr
    result = _hook(loop_world, "PreToolUse", tool_name="EnterPlanMode", tool_input={})
    assert result.returncode == 2, result.stdout + result.stderr
    decision = json.loads(result.stdout)["hookSpecificOutput"]
    assert decision["permissionDecision"] == "deny"
    assert DENIED in decision["permissionDecisionReason"]


def _plan(world: tuple[str, dict[str, str], Path], **fields: object) -> int:
    return _hook(world, "PreToolUse", tool_name="EnterPlanMode", tool_input={}, **fields).returncode


@pytest.mark.parametrize("prompt", ["fix this", "explain /loop", "/loophole", "don't use /loop"])
def test_plain_interactive_prompts_keep_plan_mode(
    loop_world: tuple[str, dict[str, str], Path], prompt: str
) -> None:
    assert _hook(loop_world, "UserPromptSubmit", prompt=prompt).returncode == 0
    assert _plan(loop_world) == 0


def test_loop_state_is_session_scoped_and_opt_out_wins(
    loop_world: tuple[str, dict[str, str], Path],
) -> None:
    _hook(loop_world, "UserPromptSubmit", prompt="/loop fix it")
    assert _plan(loop_world, session_id="other-session") == 0
    assert _plan(loop_world) == 2
    loop_world[1]["CLUD_ALLOW_PLAN_MODE"] = "1"
    assert _plan(loop_world) == 0


def _scheduled(world: tuple[str, dict[str, str], Path], tool: str, job: str) -> None:
    result = _hook(world, "PostToolUse", tool_name=tool,
                   tool_input={"id": job}, tool_response={"id": job})
    assert result.returncode == 0, result.stderr


def test_only_the_last_cron_delete_restores_plan_mode(
    loop_world: tuple[str, dict[str, str], Path],
) -> None:
    _scheduled(loop_world, "CronCreate", "first")
    _scheduled(loop_world, "CronCreate", "second")
    assert _plan(loop_world) == 2
    _scheduled(loop_world, "CronDelete", "first")
    assert _plan(loop_world) == 2
    _scheduled(loop_world, "CronDelete", "second")
    assert _plan(loop_world) == 0


def test_failed_schedule_and_unrelated_tools_do_not_start_a_loop(
    loop_world: tuple[str, dict[str, str], Path],
) -> None:
    _hook(loop_world, "PostToolUseFailure", tool_name="CronCreate",
          tool_input={}, tool_response={"id": "failed"})
    _scheduled(loop_world, "Read", "not-a-job")
    assert _plan(loop_world) == 0


def test_native_inventory_and_dynamic_stop_clear_scheduling_state(
    loop_world: tuple[str, dict[str, str], Path],
) -> None:
    _hook(loop_world, "PostToolUse", tool_name="ScheduleWakeup",
          tool_input={"delaySeconds": 60}, tool_response={})
    assert _plan(loop_world) == 2
    _hook(loop_world, "PostToolUse", tool_name="ScheduleWakeup",
          tool_input={"stop": True}, tool_response={})
    assert _plan(loop_world) == 0
    _scheduled(loop_world, "CronCreate", "expired")
    _hook(loop_world, "Stop", session_crons=[])
    assert _plan(loop_world) == 0


def test_an_existing_manual_plan_gets_recovery_context(
    loop_world: tuple[str, dict[str, str], Path],
) -> None:
    result = _hook(loop_world, "UserPromptSubmit", prompt="/loop fix it", permission_mode="plan")
    assert result.returncode == 0
    context = json.loads(result.stdout)["hookSpecificOutput"]["additionalContext"]
    assert "ExitPlanMode" in context
    assert "cannot force" in context


def test_session_end_releases_the_loop_marker(loop_world: tuple[str, dict[str, str], Path]) -> None:
    _hook(loop_world, "UserPromptSubmit", prompt="/loop fix it")
    _hook(loop_world, "SessionEnd", reason="prompt_input_exit")
    assert _plan(loop_world) == 0


@pytest.mark.parametrize("allowed", [False, True])
def test_openrouter_launch_carries_explicit_plan_mode_choice(
    loop_world: tuple[str, dict[str, str], Path], allowed: bool
) -> None:
    binary, env, repo = loop_world
    result = process.run(
        [binary, "--openrouter", "--model", "nvidia/nemotron-3-ultra-550b-a55b:free",
         "--dry-run", *(["--allow-plan-mode"] if allowed else [])],
        env=consent(env, "openrouter"), cwd=repo, capture_output=True, text=True, timeout=30,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    plan = json.loads(result.stdout)
    assert plan["allow_plan_mode"] is allowed
    assert not any(
        arg.startswith("--disallowedTools=") and "EnterPlanMode" in arg
        for arg in plan["command"]
    )

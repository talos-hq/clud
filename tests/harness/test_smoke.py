"""Smoke tests for the harness itself (#1323): does the real Claude Code
execute a scripted tool call, and do the logs see it?"""

from __future__ import annotations

from tests.harness.harness import Harness


def test_scripted_bash_call_runs_and_is_recorded(harness: Harness) -> None:
    script = {
        "default_text": "SMOKE_DONE",
        "roles": [
            {
                "name": "main",
                "steps": [
                    {
                        "tool_use": {
                            "name": "Bash",
                            "input": {"command": "echo harness-smoke-ok", "description": "smoke"},
                        }
                    },
                    {
                        "expect": {"is_error": False, "content_contains": "harness-smoke-ok"},
                        "text": "SMOKE_DONE",
                    },
                ],
            },
        ],
    }
    result = harness.run("run the smoke check", script)
    assert result.returncode == 0, result.stdout[-2000:]
    assert "SMOKE_DONE" in result.stdout
    assert not any("MOCK_EXPECT_FAILED" in str(r.get("step")) for r in result.requests)
    # The Bash call reached the hooks, from the main session (no agent type).
    bash = [h for h in result.hooks if h.get("tool_name") == "Bash"]
    assert bash
    assert bash[0]["agent_type"] is None
    assert bash[0]["tool_input"]["command"] == "echo harness-smoke-ok"
    # The installed clud assets are what the harness loaded.
    init = next(
        e for e in result.events if e.get("type") == "system" and e.get("subtype") == "init"
    )
    assert "grind" in init.get("slash_commands", [])
    assert "grind-editor" in [
        a if isinstance(a, str) else a.get("name") for a in init.get("agents", [])
    ]


def test_command_guard_denies_are_visible_to_the_script(harness: Harness) -> None:
    # A command the guard refuses (`find /`) comes back as an error tool_result.
    script = {
        "default_text": "DONE",
        "roles": [
            {
                "name": "main",
                "steps": [
                    {
                        "tool_use": {
                            "name": "Bash",
                            "input": {"command": "find / -name nothing", "description": "x"},
                        }
                    },
                    {"expect": {"is_error": True}, "text": "GUARD_DENIED_AS_EXPECTED"},
                ],
            },
        ],
    }
    result = harness.run("try the guarded command", script)
    assert "GUARD_DENIED_AS_EXPECTED" in result.stdout, result.stdout[-2000:]

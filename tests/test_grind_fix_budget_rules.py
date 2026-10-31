"""A lander's own tool mistake never spends a fix round (#1331)."""

from __future__ import annotations

from pathlib import Path

ASSETS = Path(__file__).resolve().parents[1] / "crates/clud-bin/assets"


def _workflow() -> str:
    return (ASSETS / "workflows/grind-run.js").read_text(encoding="utf-8")


def _function(source: str, name: str) -> str:
    start = source.index(f"const {name} =")
    end = source.find("\nconst ", start + 1)
    return source[start : end if end != -1 else len(source)]


def test_lander_status_enum_has_tool_misuse_and_a_default_cap_of_three() -> None:
    source = _workflow()
    enum = "enum: ['merged', 'enqueued', 'needs_fix', 'base_red', 'gave_up', 'tool_misuse']"
    assert enum in source
    assert "const MAX_AGENT_ERRORS = Math.max(0, Math.floor(args.agentErrors ?? 3))" in source


def test_tool_misuse_never_touches_fix_rounds_or_the_integrator() -> None:
    body = _function(_workflow(), "integrateAndLand")
    start = body.index("while (l && l.status === 'tool_misuse')")
    loop = body[start : body.index("if (l && l.status === 'merged')")]
    assert "fixes++" not in loop
    assert "fixes +" not in loop
    assert "integrate(" not in loop
    # The retry lands again with the SAME fix-round count.
    assert "land(p, g, integ.pr_url, fixes)" in loop
    # Past the allowance the goal stops, naming the command.
    assert "agentErrors > MAX_AGENT_ERRORS" in loop
    assert "gave_up: tool misuse (${l.command || l.summary})" in loop


def test_the_allowance_is_per_goal_and_separate_from_max_fix() -> None:
    body = _function(_workflow(), "integrateAndLand")
    assert body.index("let agentErrors = 0") < body.index("for (let fixes = 0")
    assert "MAX_FIX" in body
    assert "MAX_AGENT_ERRORS" not in body.split("fixes >= MAX_FIX")[1].split("\n")[0]


def test_lander_skill_maps_its_own_mistakes_to_tool_misuse() -> None:
    land = " ".join((ASSETS / "skills/grind-land/SKILL.md").read_text(encoding="utf-8").split())
    assert "return `status=tool_misuse`" in land
    assert "never counts it as a fix round" in land

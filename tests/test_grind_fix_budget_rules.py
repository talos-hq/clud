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


def test_chaperone_verdicts_have_tool_misuse_and_a_default_cap_of_three() -> None:
    source = _workflow()
    enum = (
        "enum: ['merged', 'queued', 'needs_fix', 'needs_fixup', 'stuck', 'base_red', 'tool_misuse']"
    )
    assert enum in source
    assert "const MAX_AGENT_ERRORS = Math.max(0, Math.floor(args.agentErrors ?? 3))" in source


def test_tool_misuse_never_touches_fix_rounds_or_the_integrator() -> None:
    body = _function(_workflow(), "judge")
    start = body.index("if (verdict === 'tool_misuse')")
    branch = body[start : body.index("if (verdict === 'needs_fix' &&")]
    assert "e.fixes" not in branch
    assert "e.fixups" not in branch
    assert "integrate(" not in branch
    assert "return 'watch'" in branch
    # Past the allowance the goal stops, naming the command.
    assert "e.agentErrors <= MAX_AGENT_ERRORS" in branch
    assert "gave_up: tool misuse (${v.command || v.summary})" in branch


def test_the_allowance_is_per_goal_and_separate_from_max_fix() -> None:
    body = _function(_workflow(), "handOff")
    assert "agentErrors: 0" in body
    assert "fixes: 0" in body
    judge = _function(_workflow(), "judge")
    assert "e.fixes < MAX_FIX" in judge
    assert "MAX_AGENT_ERRORS" not in judge.split("e.fixes < MAX_FIX")[1].split("\n")[0]


def test_lander_skill_maps_its_own_mistakes_to_tool_misuse() -> None:
    land = " ".join((ASSETS / "skills/grind-land/SKILL.md").read_text(encoding="utf-8").split())
    assert "return `status=tool_misuse`" in land
    assert "never counts it as a fix round" in land

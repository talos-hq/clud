"""Guard the /grind context pack in the bundled assets (#1339 items 1-2, #1344).

The planner returns the issue text and code excerpts once; the workflow puts
that pack first in every worker's and the reviewer's prompt. The
byte-identical prefix itself is proven on the real Claude Code in
`tests/harness/test_grind_context_pack.py`.
"""

from __future__ import annotations

import re
from pathlib import Path

ASSETS = Path(__file__).resolve().parents[1] / "crates/clud-bin/assets"


def _flat(text: str) -> str:
    return " ".join(text.split())


def _skill(name: str) -> str:
    return _flat((ASSETS / "skills" / name / "SKILL.md").read_text(encoding="utf-8"))


def _workflow() -> str:
    return (ASSETS / "workflows" / "grind-run.js").read_text(encoding="utf-8")


def test_the_plan_schema_carries_the_pack() -> None:
    js = _workflow()
    plan = js[js.index("const PLAN = {") : js.index("const WORK = {")]
    for field in ("context:", "issue:", "excerpts:", "text:"):
        assert field in plan, field


def test_the_pack_comes_first_in_worker_and_reviewer_prompts() -> None:
    js = _workflow()
    for procedure in ("grind-edit", "grind-review"):
        assert re.search(
            r"`\$\{packOf\(p\)\}Follow your built-in /" + procedure + " procedure", js
        ), f"{procedure} prompt must start with the pack"


def test_nothing_per_agent_enters_the_pack() -> None:
    js = _workflow()
    body = js[js.index("const packOf = (p) => {") :]
    body = body[: body.index("\n}\n")]
    for varying in ("t.id", "g.id", "round", "Date", "checkout"):
        assert varying not in body, f"{varying} would make the pack differ per agent"


def test_one_editor_takes_every_task_after_the_pack() -> None:
    """DD-205: one editor per goal, so nothing varies ahead of the pack."""
    js = _workflow()
    assert "PACK_STAGGER" not in js
    assert "`${packOf(p)}Follow your built-in /grind-edit procedure." in js


def test_the_planner_returns_the_pack_and_workers_use_it() -> None:
    plan = _skill("grind-plan")
    assert "**Context pack**" in plan
    assert "verbatim" in plan
    assert "{path, start, end, why, text}" in plan
    assert "do not re-fetch the issue" in _skill("grind-edit")
    assert "instead of re-fetching the issue" in _skill("grind-review")

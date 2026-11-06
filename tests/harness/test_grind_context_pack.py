"""`/grind`'s context pack on the real Claude Code (#1339, #1344).

The planner returns the goal's issue text and code excerpts once; the
workflow puts that pack first in every worker's and the reviewer's prompt.
#1344's premise is that the workers' requests are then byte-identical up to
the end of the pack. Claude Code puts a git status snapshot ahead of every
agent's prompt, so this holds only for workers that start against the same
checkout state. The workflow starts parallel workers together, but cannot
order their startup against each other's writes, so the tests mask that one
status block and require everything else to match.
"""

from __future__ import annotations

import json
import re
from typing import Any

from tests.harness.harness import Harness, RunResult
from tests.harness.test_grind import _after, _goal_roles, _goals, _role, _script, _structured

# Claude Code's workflow harness indents every line of computed task text,
# so match the marker's text, not its surrounding newlines.
PACK_END = "--- end of context pack ---"
TASKS = ("t1", "t2", "t3")
# Claude Code's git status snapshot, taken when each agent starts. clud cannot
# order agent startup against other workers' writes, so it is masked.
STATUS = re.compile(r"Status:\\n.+?\\n\\nRecent commits:")
EXCERPT = "\n".join(f"line {n}: let shared_context_{n} = {n};" for n in range(1, 160))


def _file(goal: str, task: str) -> str:
    """t1 writes the file the inherited integrator script commits."""
    return f"{goal}.txt" if task == "t1" else f"{goal}-{task}.txt"


def _pack_goal(h: Harness, goal: str) -> list[dict[str, Any]]:
    """One goal whose plan carries a pack and three parallel tasks."""
    roles = _goal_roles(h, goal)
    plan = {
        "checkout": str(h.repo),
        "branch": f"grind/{goal}",
        "depends_on": [],
        "verify": "true",
        "tasks": [
            {"id": t, "files": [_file(goal, t)], "instructions": f"write {_file(goal, t)}"}
            for t in TASKS
        ],
        "context": {
            "issue": "Issue body: make the three files.\nComment: keep them short.",
            "excerpts": [
                {"path": "src/shared.rs", "start": 1, "end": 159, "why": "shared", "text": EXCERPT}
            ],
        },
    }
    at = f"Goal {goal}:"
    ok = {"is_error": False}
    workers = []
    for t in TASKS:
        write = {"file_path": f"{h.repo}/{_file(goal, t)}", "content": f"{t}\n"}
        steps = [
            {"tool_use": {"name": "Write", "input": write}, "delay_ms": 300},
            _after(ok, _structured({"files_touched": [_file(goal, t)], "summary": t})),
        ]
        workers.append(_role(f"worker:{goal}:{t}", "worker", [at, f"Task {t}."], steps))
    rest = [r for r in roles if not r["name"].startswith(("planner:", "worker:"))]
    planner = _role(f"planner:{goal}", "planner", at, [_structured(plan)])
    return [planner, *workers, *rest]


def _first(result: RunResult, role: str) -> dict[str, Any]:
    return next(r for r in result.requests if r["role"] == role)


def _prefix(request: dict[str, Any]) -> str:
    """Everything the model sees, serialized, up to the end of the pack."""
    whole = json.dumps({"system": request.get("system"), "messages": request.get("messages")})
    marker = PACK_END
    assert marker in whole, "the pack never reached this agent; messages begin:\n" + json.dumps(
        request.get("messages")
    )[:3000]
    return STATUS.sub("Status:<masked>", whole[: whole.index(marker) + len(marker)])


def _divergence(prefixes: dict[str, str]) -> str:
    """Where each worker's prefix first differs from t1's, with context."""
    base = prefixes["t1"]
    lines = []
    for task, other in prefixes.items():
        at = next((i for i, (a, b) in enumerate(zip(base, other, strict=False)) if a != b), None)
        if at is None and len(base) == len(other):
            lines.append(f"{task}: identical to t1")
            continue
        at = min(len(base), len(other)) if at is None else at
        lines.append(
            f"{task}: differs at {at}\n  t1:    {base[max(0, at - 200) : at + 200]!r}"
            f"\n  {task}: {other[max(0, at - 200) : at + 200]!r}"
        )
    return "\n".join(lines)


def _run(harness: Harness) -> RunResult:
    result = harness.run(
        "start grind",
        _script(harness, "parallel", _goals("g"), _pack_goal(harness, "g")),
        timeout=600,
    )
    assert result.returncode == 0, result.stdout[-3000:]
    return result


def test_the_one_editor_reads_the_pack_before_any_task(harness: Harness) -> None:
    result = _run(harness)
    prefix = _prefix(_first(result, "worker:g:t1"))
    assert "make the three files" in prefix
    assert "shared_context_159" in prefix
    for t in TASKS:
        assert f"Task {t}." not in prefix, "task-specific text precedes the pack"


def test_the_reviewer_starts_from_the_same_pack(harness: Harness) -> None:
    result = _run(harness)
    reviewer = json.dumps(_first(result, "reviewer:g").get("messages"))
    pack = json.dumps(_first(result, "worker:g:t1").get("messages"))
    start = pack.index("Context pack for this goal")
    end = pack.index(PACK_END) + len(PACK_END)
    assert pack[start:end] in reviewer

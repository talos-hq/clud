"""grind's merge-queue landing pipeline (#2467, DD-192).

A goal is done once its PR is enqueued; a checkout-free chaperone follows
each queued PR, and the workflow sends an ejected PR back to the integrator
ahead of new work. These tests execute the production `grind-run.js` body in
the controlled host `tests/fixtures/grind_workflow_probe.mjs` with scripted
agent replies, so they cover the workflow's data flow, not real agents.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from tests import process

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / "crates/clud-bin/assets/workflows/grind-run.js"
PROBE = ROOT / "tests/fixtures/grind_workflow_probe.mjs"
ASSETS = ROOT / "crates/clud-bin/assets"

PUSHED = {"pushed": True, "pr_url": "https://github.com/o/r/pull/34", "summary": "rebased"}
ENQUEUED = {"status": "enqueued", "summary": "green; enqueued", "head_sha": "a" * 40}


def _verdict(verdict: str, **extra: Any) -> dict[str, Any]:
    return {"verdict": verdict, "summary": verdict, **extra}


def probe(tmp_path: Path, script: dict[str, list[Any]], goals: tuple[str, ...] = ("g1",)) -> dict:
    args = {
        "repo": str(tmp_path),
        "github_repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "goals": [{"id": g, "title": g, "brief": g} for g in goals],
    }
    config = tmp_path / "probe.json"
    config.write_text(json.dumps({"args": args, "script": script}), encoding="utf-8")
    result = process.run(
        ["node", str(PROBE), str(WORKFLOW), str(config)],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 0, result.stderr
    out = json.loads(result.stdout)
    assert "error" not in out, out
    return out


def _calls(out: dict) -> list[str]:
    return [p["label"] for p in out["prompts"] if "label" in p]


def _index(events: list[dict], key: str, label: str) -> int:
    return next(i for i, e in enumerate(events) if e.get(key, "").startswith(label))


def _goal(out: dict, goal: str) -> dict:
    return next(s for s in out["result"] if s["goal"] == goal)


def test_goal_is_done_at_enqueue_and_the_next_goal_starts_before_the_merge(tmp_path: Path) -> None:
    out = probe(
        tmp_path,
        {
            "land:g1": [ENQUEUED],
            "land:g2": [ENQUEUED],
            "chaperone:g1": [{"delay_ms": 300, "value": _verdict("merged", exit_code=0)}],
            "chaperone:g2": [_verdict("merged", exit_code=0)],
        },
        goals=("g1", "g2"),
    )
    events = out["prompts"]
    assert _index(events, "label", "plan:g2") < _index(events, "done", "chaperone:g1")
    assert _goal(out, "g1")["merged"] is True
    assert _goal(out, "g1")["status"] == "merged"
    lander = next(p["prompt"] for p in events if p.get("label", "").startswith("land:g1"))
    assert "pr_merge.py" in lander
    assert "--match-head-commit" in lander
    chaperone = next(p for p in events if p.get("label", "").startswith("chaperone:g1"))
    assert chaperone["role"] == "grind-chaperone"
    assert "--until merged" in chaperone["prompt"]
    assert "--timeout" in chaperone["prompt"]


def test_an_ejected_pr_goes_back_to_the_integrator_ahead_of_new_work(tmp_path: Path) -> None:
    out = probe(
        tmp_path,
        {
            "land:g1": [ENQUEUED, ENQUEUED],
            "land:g2": [ENQUEUED],
            "chaperone:g1": [
                _verdict("needs_fixup", exit_code=13, failure_log="ejected: conflict"),
                _verdict("merged", exit_code=0),
            ],
            "chaperone:g2": [_verdict("merged", exit_code=0)],
            "fixup:g1": [PUSHED],
        },
        goals=("g1", "g2"),
    )
    calls = _calls(out)
    assert calls.index("fixup:g1#1") < calls.index("plan:g2")
    fixup = next(p["prompt"] for p in out["prompts"] if p.get("label") == "fixup:g1#1")
    assert "origin/main" in fixup
    assert "rebase" in fixup.lower()
    assert _goal(out, "g1")["merged"] is True


def test_queue_fixups_are_capped_at_two_then_stuck(tmp_path: Path) -> None:
    pushed = PUSHED
    out = probe(
        tmp_path,
        {
            "land:g1": [ENQUEUED, ENQUEUED, ENQUEUED],
            "chaperone:g1": [_verdict("needs_fixup", exit_code=13)] * 3,
            "fixup:g1": [pushed, pushed],
        },
    )
    calls = _calls(out)
    assert [c for c in calls if c.startswith("fixup:g1")] == ["fixup:g1#1", "fixup:g1#2"]
    g1 = _goal(out, "g1")
    assert g1["merged"] is False
    assert g1["status"] == "stuck"


def test_a_pr_still_queued_at_finish_gets_one_final_wait_and_is_reported_queued(
    tmp_path: Path,
) -> None:
    out = probe(
        tmp_path,
        {
            "land:g1": [ENQUEUED],
            "chaperone:g1": [_verdict("queued", exit_code=4), _verdict("queued", exit_code=4)],
        },
    )
    assert "chaperone:g1#final" in _calls(out)
    g1 = _goal(out, "g1")
    assert g1["merged"] is False
    assert g1["status"] == "queued"
    assert not g1["note"].startswith("failed")


def test_a_red_base_costs_no_fix_round(tmp_path: Path) -> None:
    out = probe(
        tmp_path,
        {
            "land:g1": [
                {
                    "status": "base_red",
                    "summary": "Dylint also red on main run 77",
                    "failure_log": "dylint E0554",
                }
            ],
        },
    )
    calls = _calls(out)
    assert [c for c in calls if c.startswith("integrate:")] == ["integrate:g1"]
    assert not [c for c in calls if c.startswith("chaperone:")]
    assert _goal(out, "g1")["status"] == "base-red"


def test_a_child_covered_by_an_existing_pr_is_skipped_at_plan_time(tmp_path: Path) -> None:
    out = probe(
        tmp_path,
        {
            "plan:g1": [
                {
                    "checkout": str(tmp_path),
                    "branch": "grind/g1",
                    "depends_on": [],
                    "verify": "true",
                    "tasks": [],
                    "covered_by": "#2446",
                }
            ],
        },
    )
    calls = _calls(out)
    assert not [c for c in calls if c.startswith(("work:", "integrate:", "land:"))]
    g1 = _goal(out, "g1")
    assert g1["status"] == "covered"
    assert g1["note"] == "covered by #2446"


def test_chaperone_agent_runs_on_haiku_with_no_checkout_tools() -> None:
    body = (ASSETS / "agents/grind-chaperone.md").read_text(encoding="utf-8")
    front = body.split("---")[1]
    assert "model: haiku" in front
    assert "tools: Bash" in front
    for verdict in ("MERGED", "NEEDS-FIXUP", "STUCK"):
        assert verdict in body

"""grind's editor -> integrator -> N chaperones shape (DD-205).

One editor and one integrator at a time, the integrator returns at push, and
one large-model chaperone per PR watches checks and the queue and fixes up
itself in a bounded, recycled worktree pool. These tests execute the production
`grind-run.js` in the controlled host `tests/fixtures/grind_workflow_probe.mjs`
with scripted replies: they cover scheduling and data flow, not real agents or
a real Claude Workflow.
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any

from tests import process

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / "crates/clud-bin/assets/workflows/grind-run.js"
PROBE = ROOT / "tests/fixtures/grind_workflow_probe.mjs"
ASSETS = ROOT / "crates/clud-bin/assets"
HEAD = "b" * 40


def _slow(value: Any, ms: int = 80) -> dict[str, Any]:
    return {"delay_ms": ms, "value": value}


def _verdict(verdict: str, **extra: Any) -> dict[str, Any]:
    return {"verdict": verdict, "summary": verdict, **extra}


MERGED = _verdict("merged", exit_code=0)
FIXED = {"pushed": True, "summary": "fixed", "head_sha": HEAD}


def probe(
    tmp_path: Path,
    script: dict[str, list[Any]],
    goals: tuple[str, ...] = ("g1",),
    mode: str = "sequential",
    **extra: Any,
) -> dict:
    args = {
        "repo": str(tmp_path / "proj"),
        "github_repo": "o/r",
        "main": "main",
        "mode": mode,
        "goals": [{"id": g, "title": g, "brief": g} for g in goals],
        **extra,
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


def _peak(out: dict, prefix: str) -> int:
    """The most agents whose label starts with `prefix` that ran at once."""
    live = peak = 0
    for event in out["prompts"]:
        if event.get("label", "").startswith(prefix):
            live += 1
            peak = max(peak, live)
        elif event.get("done", "").startswith(prefix):
            live -= 1
    return peak


def _at(out: dict, key: str, label: str) -> int:
    return next(i for i, e in enumerate(out["prompts"]) if e.get(key, "").startswith(label))


def _prompts(out: dict, label: str) -> list[str]:
    return [p["prompt"] for p in out["prompts"] if p.get("label", "").startswith(label)]


def _goal(out: dict, goal: str) -> dict:
    return next(s for s in out["result"] if s["goal"] == goal)


def test_the_editor_never_runs_concurrently_and_gets_every_task_of_its_goal(
    tmp_path: Path,
) -> None:
    plan = {
        "checkout": str(tmp_path),
        "branch": "grind/x",
        "depends_on": [],
        "verify": "true",
        "tasks": [
            {"id": "a", "files": ["a.rs"], "instructions": "do a"},
            {"id": "b", "files": ["b.rs"], "instructions": "do b"},
        ],
    }
    edit = {"files_touched": ["a.rs", "b.rs"], "summary": "edited"}
    out = probe(
        tmp_path,
        {"plan:": [plan, plan, plan], "edit:": [_slow(edit), _slow(edit), _slow(edit)]},
        goals=("g1", "g2", "g3"),
        mode="parallel",
    )
    assert _peak(out, "edit:") == 1
    assert len(_prompts(out, "edit:g1")) == 1
    prompt = _prompts(out, "edit:g1")[0]
    assert "do a" in prompt
    assert "do b" in prompt


def test_the_integrator_never_runs_concurrently(tmp_path: Path) -> None:
    pushed = {"pushed": True, "pr_url": "https://github.com/o/r/pull/1", "summary": "pushed"}
    out = probe(
        tmp_path,
        {"integrate:": [_slow(pushed), _slow(pushed), _slow(pushed)]},
        goals=("g1", "g2", "g3"),
        mode="parallel",
    )
    assert _peak(out, "integrate:") == 1


def test_the_integrator_must_pass_the_multi_platform_lint_gate_and_no_full_test_suite(
    tmp_path: Path,
) -> None:
    out = probe(tmp_path, {}, scripts={"lint": "./lint", "test": "./test"})
    prompt = _prompts(out, "integrate:g1")[0]
    assert "MULTI-PLATFORM LINT GATE" in prompt
    assert "workspace.metadata.soldr" in prompt
    assert "./lint" in prompt
    assert "./test" not in prompt
    assert "Return as soon as the PR is pushed" in prompt


def test_nothing_waits_for_ci_before_the_next_goal_starts(tmp_path: Path) -> None:
    out = probe(
        tmp_path,
        {"chaperone:g1": [_slow(MERGED, 300)], "chaperone:g2": [MERGED]},
        goals=("g1", "g2"),
    )
    assert _at(out, "done", "integrate:g1") < _at(out, "label", "chaperone:g1")
    assert _at(out, "label", "plan:g2") < _at(out, "done", "chaperone:g1")
    assert not [p for p in out["prompts"] if p.get("role") == "grind-lander"]
    assert _goal(out, "g1")["merged"] is True


def test_many_prs_are_chaperoned_at_once_while_the_next_goal_proceeds(tmp_path: Path) -> None:
    out = probe(
        tmp_path,
        {
            "chaperone:g1": [_slow(MERGED, 400)],
            "chaperone:g2": [_slow(MERGED, 400)],
            "chaperone:g3": [_slow(MERGED, 400)],
        },
        goals=("g1", "g2", "g3"),
    )
    assert _peak(out, "chaperone:") == 3
    assert all(_goal(out, g)["merged"] is True for g in ("g1", "g2", "g3"))


def test_a_chaperone_prompt_waits_for_checks_then_enqueues_pinned_then_follows_the_queue(
    tmp_path: Path,
) -> None:
    out = probe(tmp_path, {})
    prompt = _prompts(out, "chaperone:g1")[0]
    checks = prompt.index("--no-merge-wait")
    enqueue = prompt.index("github/pr_merge.py")
    follow = prompt.index("--until merged")
    assert checks < enqueue < follow
    assert "--match-head-commit" in prompt


def test_chaperones_fix_up_themselves_without_the_integrator_lock(tmp_path: Path) -> None:
    pushed = {"pushed": True, "pr_url": "https://github.com/o/r/pull/2", "summary": "pushed"}
    out = probe(
        tmp_path,
        {
            "chaperone:g1": [_verdict("needs_fixup", failure_log="ejected: conflict"), MERGED],
            "chaperone-fix:g1": [_slow(FIXED, 400)],
            "integrate:g2": [_slow(pushed, 50)],
        },
        goals=("g1", "g2"),
    )
    fix_start = _at(out, "label", "chaperone-fix:g1")
    fix_done = _at(out, "done", "chaperone-fix:g1")
    assert fix_start < _at(out, "done", "integrate:g2") < fix_done
    fix = next(p for p in out["prompts"] if p.get("label", "").startswith("chaperone-fix:g1"))
    assert fix["role"] == "grind-chaperone"
    assert "ejected: conflict" in fix["prompt"]
    assert _prompts(out, "integrate:g1")
    assert len(_prompts(out, "integrate:g1")) == 1
    assert _goal(out, "g1")["merged"] is True
    second_watch = _prompts(out, "chaperone:g1")[1]
    assert HEAD in second_watch


def test_the_fixup_pool_is_bounded_and_recycles_a_warm_worktree(tmp_path: Path) -> None:
    fails = _verdict("needs_fixup", failure_log="ejected")
    goals = ("g1", "g2", "g3", "g4")
    script: dict[str, list[Any]] = {f"chaperone:{g}": [fails, MERGED] for g in goals}
    script["chaperone-fix:"] = [_slow(FIXED) for _ in goals]
    out = probe(tmp_path, script, goals=goals, mode="parallel", fixupPool=2)
    assert _peak(out, "chaperone-fix:") == 2
    slots = re.findall(
        r"worktrees/proj-wt-grind-fixup-(\d+)", "\n".join(_prompts(out, "chaperone-fix:"))
    )
    assert set(slots) == {"1", "2"}
    prompt = _prompts(out, "chaperone-fix:")[0]
    assert "worktree add --detach" in prompt
    assert "clean -fd" in prompt
    assert "clean -fdx" not in prompt
    cleanup = _prompts(out, "chaperone-cleanup")
    assert len(cleanup) == 1
    assert "proj-wt-grind-fixup-1" in cleanup[0]
    assert "proj-wt-grind-fixup-2" in cleanup[0]


def test_a_run_with_no_fixup_creates_no_worktree_and_needs_no_cleanup(tmp_path: Path) -> None:
    out = probe(tmp_path, {})
    assert not _prompts(out, "chaperone-fix:")
    assert not _prompts(out, "chaperone-cleanup")


def test_queue_fixups_are_bounded_then_stuck(tmp_path: Path) -> None:
    eject = _verdict("needs_fixup", failure_log="ejected")
    out = probe(
        tmp_path,
        {"chaperone:g1": [eject, eject, eject], "chaperone-fix:g1": [FIXED, FIXED]},
    )
    assert len(_prompts(out, "chaperone-fix:g1")) == 2
    g1 = _goal(out, "g1")
    assert g1["merged"] is False
    assert g1["status"] == "stuck"


def test_red_checks_are_bounded_by_fix_rounds_then_stuck(tmp_path: Path) -> None:
    red = _verdict("needs_fix", failure_log="test failed")
    out = probe(
        tmp_path,
        {"chaperone:g1": [red, red], "chaperone-fix:g1": [FIXED]},
        maxFixRounds=1,
    )
    assert len(_prompts(out, "chaperone-fix:g1")) == 1
    assert _goal(out, "g1")["status"] == "stuck"


def test_an_unpushed_fixup_is_stuck(tmp_path: Path) -> None:
    out = probe(
        tmp_path,
        {
            "chaperone:g1": [_verdict("needs_fix", failure_log="red")],
            "chaperone-fix:g1": [{"pushed": False, "summary": "no", "failure_log": "cannot fix"}],
        },
    )
    g1 = _goal(out, "g1")
    assert g1["status"] == "stuck"
    assert "cannot fix" in g1["note"]


def test_a_red_base_costs_no_fixup(tmp_path: Path) -> None:
    out = probe(tmp_path, {"chaperone:g1": [_verdict("base_red", failure_log="dylint on main")]})
    assert not _prompts(out, "chaperone-fix:")
    assert _goal(out, "g1")["status"] == "base-red"


def test_a_chaperone_tool_mistake_spends_no_fix_round_and_is_bounded(tmp_path: Path) -> None:
    misuse = _verdict("tool_misuse")
    out = probe(tmp_path, {"chaperone:g1": [misuse, misuse]}, agentErrors=1)
    assert not _prompts(out, "chaperone-fix:")
    assert "gave_up: tool misuse" in _goal(out, "g1")["note"]


def test_a_dependent_starts_integrating_only_after_its_dependency_merged(tmp_path: Path) -> None:
    def planned(depends: list[str], name: str) -> dict[str, Any]:
        return {
            "checkout": str(tmp_path),
            "branch": f"grind/{name}",
            "depends_on": depends,
            "verify": "true",
            "tasks": [{"id": "t", "files": ["a.rs"], "instructions": "do"}],
        }

    out = probe(
        tmp_path,
        {
            "plan:g1": [planned([], "g1")],
            "plan:g2": [planned(["g1"], "g2")],
            "chaperone:g1": [_slow(MERGED, 200)],
        },
        goals=("g1", "g2"),
        mode="parallel",
    )
    assert _at(out, "done", "chaperone:g1") < _at(out, "label", "integrate:g2")
    assert _goal(out, "g2")["merged"] is True


def test_a_sequential_dependent_waits_for_a_dependency_still_in_its_chaperone(
    tmp_path: Path,
) -> None:
    def planned(depends: list[str], name: str) -> dict[str, Any]:
        return {
            "checkout": str(tmp_path),
            "branch": f"grind/{name}",
            "depends_on": depends,
            "verify": "true",
            "tasks": [{"id": "t", "files": ["a.rs"], "instructions": "do"}],
        }

    out = probe(
        tmp_path,
        {
            "plan:g1": [planned([], "g1")],
            "plan:g2": [planned(["g1"], "g2")],
            "chaperone:g1": [_slow(MERGED, 200)],
        },
        goals=("g1", "g2"),
    )
    assert _at(out, "done", "chaperone:g1") < _at(out, "label", "integrate:g2")
    assert _goal(out, "g2")["merged"] is True


def test_a_dependent_is_blocked_when_its_dependency_gets_stuck(tmp_path: Path) -> None:
    def planned(depends: list[str]) -> dict[str, Any]:
        return {
            "checkout": str(tmp_path),
            "branch": "grind/x",
            "depends_on": depends,
            "verify": "true",
            "tasks": [{"id": "t", "files": ["a.rs"], "instructions": "do"}],
        }

    out = probe(
        tmp_path,
        {
            "plan:g1": [planned([])],
            "plan:g2": [planned(["g1"])],
            "chaperone:g1": [_slow(_verdict("stuck"), 100)],
        },
        goals=("g1", "g2"),
        mode="parallel",
    )
    assert not _prompts(out, "integrate:g2")
    assert _goal(out, "g2")["blocked"] is True


def test_chaperones_use_the_large_model_with_the_handoff_and_no_inherited_cheap_model(
    tmp_path: Path,
) -> None:
    pushed = {
        "pushed": True,
        "pr_url": "https://github.com/o/r/pull/9",
        "summary": "pushed",
        "head_sha": HEAD,
        "handoff": "INTEGRATOR-HANDOFF-NOTE",
    }
    out = probe(
        tmp_path,
        {
            "integrate:g1": [pushed],
            "chaperone:g1": [_verdict("needs_fix", failure_log="red"), MERGED],
            "chaperone-fix:g1": [FIXED],
        },
    )
    for label in ("chaperone:g1", "chaperone-fix:g1"):
        call = next(p for p in out["prompts"] if p.get("label", "").startswith(label))
        assert call["model"] == "opus"
        assert "INTEGRATOR-HANDOFF-NOTE" in call["prompt"]
        assert "pull/9" in call["prompt"]
        assert HEAD in call["prompt"]


def test_the_handoff_falls_back_to_the_plan_when_the_integrator_wrote_none(
    tmp_path: Path,
) -> None:
    out = probe(tmp_path, {})
    prompt = _prompts(out, "chaperone:g1")[0]
    assert "controlled push" in prompt
    assert "grind/12" in prompt


def test_a_session_override_can_pick_the_chaperone_model(tmp_path: Path) -> None:
    out = probe(tmp_path, {}, models={"chaperone": "fable"})
    call = next(p for p in out["prompts"] if p.get("label", "").startswith("chaperone:g1"))
    assert call["model"] == "fable"


def test_a_feature_stage_goal_has_a_chaperone_merge_into_the_feature_branch_without_a_queue(
    tmp_path: Path,
) -> None:
    out = probe(
        tmp_path,
        {},
        feature={"branch": "grind/meta-1-x", "worktree": str(tmp_path), "pr": "77"},
        plan={
            "schema": "grind-plan/v1",
            "run_id": "x",
            "meta": "1",
            "repo": "o/r",
            "stages": [{"stage": "feature", "group": "x", "children": ["g1"]}],
        },
    )
    prompt = _prompts(out, "chaperone:g1")[0]
    assert "gh pr merge <n> --admin --merge --repo o/r" in prompt
    assert "github/pr_merge.py" not in prompt
    assert "Closes #g1" in prompt


def test_a_pr_still_queued_at_finish_gets_one_final_wait_and_is_reported_queued(
    tmp_path: Path,
) -> None:
    queued = _verdict("queued", exit_code=4, enqueued=True)
    out = probe(tmp_path, {"chaperone:g1": [queued, queued, queued, queued]})
    calls = [p["label"] for p in out["prompts"] if "label" in p]
    assert "chaperone:g1#final" in calls
    g1 = _goal(out, "g1")
    assert g1["merged"] is False
    assert g1["status"] == "queued"
    assert not g1["note"].startswith("failed")
    final = next(p["prompt"] for p in out["prompts"] if p.get("label") == "chaperone:g1#final")
    assert "already in the queue" in final
    assert "github/pr_merge.py" not in final


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
    assert not _prompts(out, "edit:")
    assert not _prompts(out, "integrate:")
    g1 = _goal(out, "g1")
    assert g1["status"] == "covered"
    assert g1["note"] == "covered by #2446"


def test_chaperone_agent_is_large_model_with_a_worktree_capable_toolset() -> None:
    body = (ASSETS / "agents/grind-chaperone.md").read_text(encoding="utf-8")
    front = body.split("---")[1]
    assert "haiku" not in front
    assert "model: opus" in front
    for tool in ("Read", "Edit", "Write", "Bash"):
        assert tool in front
    for verdict in ("MERGED", "NEEDS-FIXUP", "STUCK"):
        assert verdict in body

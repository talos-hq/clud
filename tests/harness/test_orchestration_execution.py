"""Approved run dependencies must gate planning, independent of planner prose."""
from __future__ import annotations

import json
import re

import pytest

from tests.harness.harness import Harness
from tests.harness.test_grind import (
    _after,
    _bash,
    _by_role,
    _goal_roles,
    _goals,
    _merged,
    _no_notes,
    _script,
    _structured,
)
from tests.harness.test_grind_gate_binding import install_outcome_guard
from tests.harness.test_grind_tracks import _told


def report_upstream_artifacts(roles: list[dict]) -> None:
    upstream_lander = next(role for role in roles if role["name"] == "chaperone:a:0")
    upstream_lander["steps"][-1]["tool_use"]["input"]["artifacts"] = {
        "version": "0.9.0", "tag": "v0.9.0",
        "release_url": "https://github.com/o/r/releases/tag/v0.9.0",
    }


@pytest.mark.parametrize("upstream_succeeds", [True, False])
@pytest.mark.parametrize("mode", ["sequential", "parallel"])
@pytest.mark.parametrize("staged", [False, True])
def test_approved_merge_dependency_gates_planning(
    harness: Harness, upstream_succeeds: bool, mode: str, staged: bool,
) -> None:
    h = harness
    roles = [*_goal_roles(h, "a"), *_goal_roles(h, "b"), *_goal_roles(h, "c")]
    report_upstream_artifacts(roles)
    if not upstream_succeeds:
        planner = next(role for role in roles if role["name"] == "planner:a")
        planner["steps"] = [_structured({
            "checkout": str(h.repo), "branch": "grind/a", "depends_on": [],
            "verify": "true", "tasks": [],
        })]
    # Caller order is deliberately wrong, and every per-goal planner omits
    # depends_on. The approved graph must still control the execution order.
    graph = {
        "session_repo": "o/r",
        "session_root": str(h.repo),
        "goals": [
            {"id": "b", "repo": "o/r", "main": "main", "checkout": "local",
             "blockers": [{"goal": "a", "when": "merged"}]},
            {"id": "a", "repo": "o/r", "main": "main", "checkout": "local", "blockers": []},
            {"id": "c", "repo": "o/r", "main": "main", "checkout": "local", "blockers": []},
        ],
        "proposed_goals": [], "risks": [],
    }
    stage_args = {} if not staged else {
        "plan_url": "https://github.com/o/r/issues/100#issuecomment-1000",
        "plan": {"schema": "grind-plan/v1", "meta": 100, "stages": [
            {"stage": "bugs", "base": "main", "children": ["b", "c"]},
            {"stage": "feature", "group": "upstream", "base": "main", "children": ["a"]},
        ]},
    }
    result = h.run(
        "Execute the approved graph; do not infer dependencies from planner output.",
        _script(h, mode, _goals("b", "a", "c"), roles, orchestration=graph, **stage_args),
        timeout=300,
    )
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    _no_notes(result)
    ran = _by_role(result)
    if upstream_succeeds:
        assert max(ran["chaperone:a:0"]) < min(ran["planner:b"])
        assert _merged(h) == ["grind/a", "grind/b", "grind/c"]
        downstream = json.dumps(result.first_request("planner:b")["messages"])
        assert "Upstream results (approved blockers):" in downstream
        assert "https://github.com/o/r/pull/grind/a" in downstream
        assert 'v0.9.0' in downstream
        assert "https://github.com/o/r/releases/tag/v0.9.0" in downstream
        independent = json.dumps(result.first_request("planner:c")["messages"])
        assert "Upstream results (approved blockers):" not in independent
    else:
        assert not any(role.endswith(":b") or role.startswith("lander:b:") for role in ran)
        assert not (h.repo / "b.txt").exists()
        assert _merged(h) == ["grind/c"]


@pytest.mark.parametrize("condition", ["released", "tag", "published"])
@pytest.mark.parametrize("mode", ["sequential", "parallel"])
def test_reported_release_artifacts_do_not_open_publication_gate(
    harness: Harness, monkeypatch: pytest.MonkeyPatch, condition: str, mode: str,
) -> None:
    h = harness
    roles = [*_goal_roles(h, "a"), *_goal_roles(h, "b"), *_goal_roles(h, "c")]
    report_upstream_artifacts(roles)
    graph = {
        "session_repo": "o/r", "session_root": str(h.repo),
        "goals": [
            {"id": "a", "repo": "o/r", "main": "main", "checkout": "local", "blockers": []},
            {"id": "b", "repo": "o/r", "main": "main", "checkout": "local", "blockers": [
                {"goal": "a", "when": condition,
                 "check": "gh release view v0.9.0 --repo o/r"},
            ]},
            {"id": "c", "repo": "o/r", "main": "main", "checkout": "local", "blockers": []},
        ],
        "proposed_goals": [], "risks": [],
    }
    h.write_run_facts({"phase": "execute", "orchestration": graph})
    install_outcome_guard(h, monkeypatch)
    roles.append({"name": "gate:b", "match": "You are the /grind gate checker", "steps": [
        _bash("gh release view v0.9.0 --repo o/r"),
        _after({"is_error": True}, _structured({"ready": False, "evidence": "check failed"})),
    ]})
    result = h.run("Do not infer publication from reported artifacts.",
                   _script(h, mode, _goals("a", "b", "c"), roles, orchestration=graph),
                   timeout=300)
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    _no_notes(result)
    ran = _by_role(result)
    assert "chaperone:a:0" in ran
    assert "gate:b" in ran
    assert not any(role in ran for role in ("planner:b", "worker:b", "reviewer:b", "integrator:b"))
    assert not any(role.startswith("lander:b:") for role in ran)
    assert not (h.repo / "b.txt").exists()
    assert _merged(h) == ["grind/a", "grind/c"]


def test_successful_native_publication_check_opens_dependent_goal(
    harness: Harness, monkeypatch: pytest.MonkeyPatch,
) -> None:
    h = harness
    command = "gh release view v0.9.0 --repo o/r"
    state = h.read_gh_state()
    state["releases"] = {"v0.9.0": {"publishedAt": "2026-10-22T00:00:00Z"}}
    h.write_gh_state(state)
    graph = {
        "session_repo": "o/r", "session_root": str(h.repo),
        "goals": [
            {"id": "a", "repo": "o/r", "main": "main", "checkout": "local", "blockers": []},
            {"id": "b", "repo": "o/r", "main": "main", "checkout": "local", "blockers": [
                {"goal": "a", "when": "released", "check": command},
            ]},
        ],
        "proposed_goals": [], "risks": [],
    }
    h.write_run_facts({"phase": "execute", "orchestration": graph})
    output = install_outcome_guard(h, monkeypatch)
    roles = [*_goal_roles(h, "a"), *_goal_roles(h, "b"), {
        "name": "gate:b", "match": "You are the /grind gate checker", "steps": [
            _bash(command),
            _after({"is_error": False}, _structured({
                "ready": True, "evidence": "published release v0.9.0 exists",
            })),
        ],
    }]
    result = h.run("Execute the approved publication gate before downstream planning.",
                   _script(h, "sequential", _goals("a", "b"), roles, orchestration=graph),
                   timeout=300)
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    _no_notes(result)
    ran = _by_role(result)
    assert "gate:b" in ran, ran
    assert max(ran["chaperone:a:0"]) < min(ran["gate:b"])
    assert max(ran["gate:b"]) < min(ran["planner:b"])
    assert _merged(h) == ["grind/a", "grind/b"]
    checks = [row for row in result.hooks if row.get("agent_type") == "grind-gate"
              and row.get("tool_name") == "Bash"]
    assert [row["tool_input"]["command"] for row in checks] == [command], checks
    outcomes = [json.loads(line) for line in output.read_text().splitlines()]
    assert any(row.get("event") == "PostToolUse" and row.get("agent_type") == "grind-gate"
               and row.get("tool_name") == "Bash" for row in outcomes), outcomes


@pytest.mark.parametrize("mode", ["sequential", "parallel"])
@pytest.mark.parametrize("hard_failure_first", [None, False, True])
def test_unmet_publication_gate_defers_transitive_dependents_only(
    harness: Harness, monkeypatch: pytest.MonkeyPatch, mode: str, hard_failure_first: bool | None,
) -> None:
    h = harness
    graph = {
        "session_repo": "o/r", "session_root": str(h.repo), "goals": [
            {"id": "a", "repo": "o/r", "main": "main", "blockers": []},
            {"id": "b", "repo": "o/r", "main": "main", "blockers": [
                {"goal": "a", "when": "released", "check": "test 1 = 2"},
            ]},
            {"id": "c", "repo": "o/r", "main": "main", "blockers": [
                {"goal": "b", "when": "merged"},
            ]},
            {"id": "d", "repo": "o/r", "main": "main", "blockers": []},
        ], "proposed_goals": [], "risks": [],
    }
    roles = [*_goal_roles(h, "a"), *_goal_roles(h, "b"), *_goal_roles(h, "c"),
             *_goal_roles(h, "d"), {
                 "name": "gate:b", "match": "You are the /grind gate checker", "steps": [
                     _bash("test 1 = 2"),
                     _after({"contains": "CLUD_GATE_EXIT_"}, _structured({
                         "ready": False, "evidence": "publication unavailable",
                     })),
                 ],
             }]
    goals = _goals("a", "b", "c", "d")
    if hard_failure_first is not None:
        graph["goals"].append({"id": "failed", "repo": "o/r", "main": "main", "blockers": []})
        hard = {"goal": "failed", "when": "merged"}
        blockers = graph["goals"][2]["blockers"]
        blockers.insert(0 if hard_failure_first else len(blockers), hard)
        failed = _goal_roles(h, "failed")
        failed[0]["steps"] = [_structured({
            "checkout": str(h.repo), "branch": "grind/failed", "depends_on": [],
            "verify": "true", "tasks": [],
        })]
        roles.extend(failed)
        goals.extend(_goals("failed"))
    h.write_run_facts({"phase": "execute", "orchestration": graph})
    install_outcome_guard(h, monkeypatch)
    result = h.run("Defer only the transitive branch behind the unmet publication gate.",
                   _script(h, mode, goals, roles, orchestration=graph),
                   timeout=300)
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    _no_notes(result)
    ran = _by_role(result)
    assert "gate:b" in ran
    assert "planner:b" not in ran, ran
    assert "planner:c" not in ran, ran
    assert _merged(h) == ["grind/a", "grind/d"]
    told = _told(result)
    for goal in ("b", "c"):
        pattern = rf'goal\\*"\s*:\s*\\*"{goal}\\*".{{0,200}}?status\\*"\s*:\s*\\*"deferred'
        expected_deferred = goal == "b" or hard_failure_first is None
        assert bool(re.search(pattern, told, re.DOTALL)) == expected_deferred, told[-4000:]
    if hard_failure_first is not None:
        assert "dependency failed did not land" in told, told[-4000:]

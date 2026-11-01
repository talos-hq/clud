"""`/grind`'s `grind-run` workflow on the real Claude Code (#1324).

The main session calls the `Workflow` tool with `grind-run`, as the `/grind`
router does once its questions are answered. The workflow then spawns the
real `grind-*` agent types (planner, worker, reviewer, integrator, lander),
and the mock backend plays each one per goal. Assertions read the request
log (who ran, and when), the hook log (each tool call's `agent_type`) and the
fake GitHub (PRs created and merged).

Script note: a step's `expect` checks the tool results of the *previous*
step, so an expectation about a command sits on the step after it.
"""

from __future__ import annotations

import json
from typing import Any

from tests.harness.harness import Harness, RunResult

MARK = {
    "planner": "You are the /grind planner",
    "worker": "You are a /grind worker",
    "reviewer": "You are the /grind reviewer",
    "integrator": "You are the /grind integrator",
    "lander": "You are the /grind lander",
    "chaperone": "You are the /grind chaperone",
}


def _structured(value: dict[str, Any]) -> dict[str, Any]:
    return {"tool_use": {"name": "StructuredOutput", "input": value}}


def _bash(command: str) -> dict[str, Any]:
    return {"tool_use": {"name": "Bash", "input": {"command": command, "description": "grind"}}}


def _after(expect: dict[str, Any], step: dict[str, Any]) -> dict[str, Any]:
    """`step`, first checking the previous step's tool results."""
    return {**step, "expect": expect}


def _role(
    name: str, role: str, prompt: str | list[str], steps: list[dict[str, Any]]
) -> dict[str, Any]:
    return {"name": name, "match": MARK[role], "match_prompt": prompt, "steps": steps}


def _goal_roles(
    h: Harness,
    goal: str,
    *,
    depends_on: list[str] | None = None,
    land: list[str] | None = None,
    worker_steps: list[dict[str, Any]] | None = None,
) -> list[dict[str, Any]]:
    """Scripts for one goal's five roles.

    `land` is the lander's verdict per round (default: merged at once). A
    `needs_fix` round sends the goal back to the integrator, whose fix round
    pushes again. PRs are merged by branch name, so the scripts don't depend
    on which order the goals happened to integrate in.
    """
    branch = f"grind/{goal}"
    repo = str(h.repo)
    at = f"Goal {goal}:"
    ok = {"is_error": False}
    plan = {
        "checkout": repo,
        "branch": branch,
        "depends_on": depends_on or [],
        "verify": "true",
        "tasks": [{"id": "t1", "files": [f"{goal}.txt"], "instructions": f"write {goal}.txt"}],
    }
    write = {"file_path": f"{repo}/{goal}.txt", "content": f"{goal}\n"}
    worker = worker_steps or [
        {"tool_use": {"name": "Write", "input": write}},
        _after(ok, _structured({"files_touched": [f"{goal}.txt"], "summary": "wrote it"})),
    ]
    push = (
        f"git -C {repo} switch -q main && git -C {repo} switch -q -c {branch} && "
        f"git -C {repo} add {goal}.txt && git -C {repo} commit -q -m {goal} && "
        f"git -C {repo} push -q -u origin {branch} && gh pr create --title {goal} --head {branch}"
    )
    url = f"https://github.com/o/r/pull/{branch}"
    roles = [
        _role(f"planner:{goal}", "planner", at, [_structured(plan)]),
        _role(f"worker:{goal}", "worker", at, worker),
        _role(
            f"reviewer:{goal}", "reviewer", at, [_structured({"approved": True, "summary": "ok"})]
        ),
    ]
    rounds = land or ["merged"]
    # Fix rounds first: their prompts also contain the goal marker.
    for n in range(1, len(rounds)):
        fix = _structured({"pushed": True, "pr_url": url, "summary": f"fix {n}"})
        roles.append(
            _role(f"integrator:{goal}:fix{n}", "integrator", [at, f"FIX ROUND {n} of"], [fix])
        )
    first = [
        _bash(push),
        _after(ok, _structured({"pushed": True, "pr_url": url, "summary": "pushed"})),
    ]
    roles.append(_role(f"integrator:{goal}", "integrator", at, first))
    for n, verdict in enumerate(rounds):
        if verdict == "merged":
            steps = [
                _bash(f"gh pr merge {branch} --admin --squash"),
                _after(ok, _structured({"status": "merged", "summary": "green"})),
            ]
        else:
            steps = [
                _structured({"status": verdict, "summary": "red", "failure_log": "test_x failed"})
            ]
        roles.append(_role(f"lander:{goal}:{n}", "lander", [at, f"Fix rounds used: {n} of"], steps))
    return roles


def _script(
    h: Harness, mode: str, goals: list[dict[str, Any]], roles: list[dict[str, Any]], **args: Any
) -> dict[str, Any]:
    workflow_args = {
        "repo": str(h.repo),
        "github_repo": (args.get("plan") or {}).get("repo", "o/r"),
        "main": "main",
        "mode": mode,
        "goals": goals,
        **args,
    }
    call = {"name": "grind-run", "args": workflow_args}
    main = {
        "name": "main",
        "steps": [{"tool_use": {"name": "Workflow", "input": call}}, {"text": "GRIND_STARTED"}],
    }
    return {"default_text": "OK", "roles": [*roles, main]}


def _goals(*ids: str) -> list[dict[str, Any]]:
    return [{"id": g, "title": g, "brief": g} for g in ids]


def _by_role(result: RunResult) -> dict[str, list[int]]:
    seen: dict[str, list[int]] = {}
    for request in result.requests:
        seen.setdefault(request["role"], []).append(request["n"])
    return seen


def _merged(h: Harness) -> list[str]:
    prs = h.read_gh_state().get("prs", [])
    return sorted(p["head"] for p in prs if p.get("state") == "MERGED")


def _no_notes(result: RunResult) -> None:
    notes = [(r["role"], r["note"]) for r in result.requests if r.get("note")]
    assert not notes, notes


def _intervals(result: RunResult) -> dict[str, tuple[int, int]]:
    """Each scripted role's lifetime: first request start to last reply end."""
    spans: dict[str, tuple[int, int]] = {}
    for r in result.requests:
        lo, hi = spans.get(r["role"], (r["t_start"], r["t_end"]))
        spans[r["role"]] = (min(lo, r["t_start"]), max(hi, r["t_end"]))
    return spans


def _max_overlap(spans: list[tuple[int, int]]) -> int:
    edges = sorted([(s, 1) for s, _ in spans] + [(e, -1) for _, e in spans])
    live = peak = 0
    for _, delta in edges:
        live += delta
        peak = max(peak, live)
    return peak


def test_sequential_two_goals_land_in_order(harness: Harness) -> None:
    roles = _goal_roles(harness, "g1") + _goal_roles(harness, "g2")
    result = harness.run(
        "start grind", _script(harness, "sequential", _goals("g1", "g2"), roles), timeout=300
    )
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)
    assert _merged(harness) == ["grind/g1", "grind/g2"]
    order = _by_role(result)
    g1_last = max(n for role, ns in order.items() if ":g1" in role for n in ns)
    assert g1_last < min(order["planner:g2"])
    types = {
        h["agent_type"]
        for h in result.hooks
        if h.get("event") == "PreToolUse" and h.get("agent_type")
    }
    assert {"grind-planner", "grind-worker", "grind-integrator", "grind-lander"} <= types


def test_parallel_caps_light_agents_at_four_and_integrators_at_one(harness: Harness) -> None:
    ids = [f"p{i}" for i in range(6)]
    roles: list[dict[str, Any]] = []
    for g in ids:
        roles += _goal_roles(harness, g)
    for role in roles:
        if role["name"].split(":")[0] in ("planner", "worker", "reviewer"):
            for step in role["steps"]:
                step["delay_ms"] = 400
    result = harness.run(
        "start grind", _script(harness, "parallel", _goals(*ids), roles), timeout=600
    )
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)
    assert _merged(harness) == [f"grind/{g}" for g in ids]
    spans = _intervals(result)
    light = [v for k, v in spans.items() if k.split(":")[0] in ("planner", "worker", "reviewer")]
    peak = _max_overlap(light)
    assert 2 <= peak <= 4, f"plan/work/review concurrency was {peak}"
    integrators = [v for k, v in spans.items() if k.startswith("integrator:")]
    assert _max_overlap(integrators) == 1


def test_dependent_goal_integrates_after_its_dependency_merges(harness: Harness) -> None:
    roles = _goal_roles(harness, "a") + _goal_roles(harness, "b", depends_on=["a"])
    result = harness.run(
        "start grind", _script(harness, "parallel", _goals("a", "b"), roles), timeout=300
    )
    assert result.returncode == 0, result.stdout[-3000:]
    assert _merged(harness) == ["grind/a", "grind/b"]
    spans = _intervals(result)
    assert spans["integrator:b"][0] >= spans["lander:a:0"][1]


def test_fix_round_then_merge(harness: Harness) -> None:
    roles = _goal_roles(harness, "f", land=["needs_fix", "merged"])
    result = harness.run(
        "start grind", _script(harness, "sequential", _goals("f"), roles), timeout=300
    )
    assert result.returncode == 0, result.stdout[-3000:]
    assert "integrator:f:fix1" in _by_role(result)
    assert _merged(harness) == ["grind/f"]


def test_fix_rounds_are_capped_and_the_pr_stays_open(harness: Harness) -> None:
    roles = _goal_roles(harness, "c", land=["needs_fix", "needs_fix"])
    script = _script(harness, "sequential", _goals("c"), roles, maxFixRounds=1)
    result = harness.run("start grind", script, timeout=300)
    assert result.returncode == 0, result.stdout[-3000:]
    assert _merged(harness) == []
    assert [p["state"] for p in harness.read_gh_state()["prs"]] == ["OPEN"]
    assert "lander:c:1" in _by_role(result)


def test_worker_shell_is_capped_by_clud_hook(harness: Harness) -> None:
    repo = str(harness.repo)
    denied = {"is_error": True, "content_contains": "/grind role caps"}
    write = {"file_path": f"{repo}/w.txt", "content": "w\n"}
    worker = [
        _bash("cargo build"),
        _after(denied, _bash("gh pr merge 5 --admin")),
        _after(denied, {"tool_use": {"name": "Write", "input": write}}),
        _after(
            {"is_error": False}, _structured({"files_touched": ["w.txt"], "summary": "wrote it"})
        ),
    ]
    roles = _goal_roles(harness, "w", worker_steps=worker)
    result = harness.run(
        "start grind", _script(harness, "sequential", _goals("w"), roles), timeout=300
    )
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)
    bash = [
        h
        for h in result.hooks
        if h.get("agent_type") == "grind-worker" and h.get("tool_name") == "Bash"
    ]
    assert [h["tool_input"]["command"] for h in bash] == ["cargo build", "gh pr merge 5 --admin"]
    assert _merged(harness) == ["grind/w"]


def test_a_dead_planner_fails_its_goal_and_its_dependent_without_hanging(harness: Harness) -> None:
    roles = _goal_roles(harness, "x") + _goal_roles(harness, "y", depends_on=["x"])
    for role in roles:
        if role["name"] == "planner:x":
            role["steps"] = [{"error": {"status": 400}}]
    result = harness.run(
        "start grind", _script(harness, "parallel", _goals("x", "y"), roles), timeout=300
    )
    assert result.returncode == 0, result.stdout[-3000:]
    ran = _by_role(result)
    assert "integrator:x" not in ran
    assert "integrator:y" not in ran
    assert _merged(harness) == []
    told = json.dumps([r["messages"] for r in result.requests if r["role"] == "main"])
    assert "planner died" in told
    assert "dependency x did not land" in told


def test_each_agent_runs_as_its_type_with_its_procedure(harness: Harness) -> None:
    roles = _goal_roles(harness, "r")
    result = harness.run(
        "start grind", _script(harness, "sequential", _goals("r"), roles), timeout=300
    )
    assert result.returncode == 0, result.stdout[-3000:]
    first: dict[str, dict[str, Any]] = {}
    for r in result.requests:
        first.setdefault(r["role"].split(":")[0], r)
    leaf = {
        "planner": "grind-plan",
        "worker": "grind-work",
        "reviewer": "grind-review",
        "integrator": "grind-integrate",
        "lander": "grind-land",
    }
    for role, skill in leaf.items():
        assert f"## Procedure (`/{skill}`" in first[role]["system"], role
        tools = set(first[role]["tools"])
        assert "Agent" not in tools, (role, tools)
        assert "Workflow" not in tools, (role, tools)
    assert "Edit" not in set(first["lander"]["tools"])
    assert "Bash" in set(first["worker"]["tools"])


def test_the_main_session_cannot_delegate_to_a_grind_role(harness: Harness) -> None:
    call = {"subagent_type": "grind-worker", "description": "x", "prompt": "write a file"}
    refused = {"is_error": True, "content_contains": "internal /grind role"}
    script = {
        "default_text": "OK",
        "roles": [
            {
                "name": "main",
                "steps": [
                    {"tool_use": {"name": "Agent", "input": call}},
                    _after(refused, {"text": "REFUSED"}),
                ],
            }
        ],
    }
    result = harness.run("delegate it", script)
    assert "REFUSED" in result.stdout, result.stdout[-2000:]
    assert not [r for r in result.requests if r["role"] != "main"]


def test_router_skill_carries_the_bosn_and_ci_preconditions(harness: Harness) -> None:
    # The router is explicit-only: type the slash command as the user would.
    # A model-side Skill call is intentionally refused (#1510).
    script = {
        "default_text": "OK",
        "roles": [{"name": "main", "steps": [{"text": "READ"}]}],
    }
    result = harness.run("/grind https://github.com/o/r/issues/1", script)
    assert "READ" in result.stdout, result.stdout[-2000:]
    context = result.prompt_text("main")
    assert "bosn not installed; local CI replay skipped" in context
    assert ".github/workflows/ci.yml" in context
    assert "Finish (always, as the very last step)" in context
    assert "git pull --ff-only origin <main>" in context


def test_goal_ends_at_enqueue_and_the_chaperone_reports_the_merge(harness: Harness) -> None:
    """#2467: the lander enqueues and the next goal starts; a haiku chaperone follows."""
    roles = _goal_roles(harness, "q1") + _goal_roles(harness, "q2")
    for role in roles:
        if role["name"].startswith("lander:"):
            goal = role["name"].split(":")[1]
            role["steps"] = [
                _bash(f"gh pr merge grind/{goal} --admin --squash"),
                _after(
                    {"is_error": False},
                    _structured({"status": "enqueued", "summary": "green", "head_sha": "a" * 40}),
                ),
            ]
    for goal in ("q1", "q2"):
        roles.append(
            _role(
                f"chaperone:{goal}",
                "chaperone",
                f"Goal {goal}:",
                [_structured({"verdict": "merged", "exit_code": 0, "summary": "merged"})],
            )
        )
    result = harness.run(
        "start grind", _script(harness, "sequential", _goals("q1", "q2"), roles), timeout=300
    )
    assert result.returncode == 0, result.stdout[-3000:]
    ran = _by_role(result)
    assert "chaperone:q1" in ran
    assert "chaperone:q2" in ran
    assert "--until merged" in result.prompt_text("chaperone:q1")
    assert _merged(harness) == ["grind/q1", "grind/q2"]

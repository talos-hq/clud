"""Run-plan validation on the real pinned Workflow runtime (#1338, V1-V6).

These cases deliberately use plan-only mode: invalid orchestration input must
be rejected before even the read-only classification planner is started.
Filesystem checkout safety and repository hook context need separate probes.
"""

from __future__ import annotations

import copy
import shlex
from typing import Any

import pytest

from tests.harness.harness import Harness
from tests.harness.test_grind_plan_only import (
    _after,
    _bash,
    _classify,
    _no_notes,
    _path,
    _plan_phase,
    _script,
    _seed,
    _structured,
    _told,
)


def run_plan() -> dict[str, Any]:
    return {
        "session_repo": "FastLED/FastLED",
        "goals": [
            {"id": "reld-208", "repo": "zackees/reld", "main": "main",
             "checkout": "worktree", "blockers": []},
            {"id": "reld-209", "repo": "zackees/reld", "main": "main",
             "blockers": [{"goal": "reld-208", "when": "merged"}]},
            {"id": "4642", "repo": "FastLED/FastLED", "main": "master",
             "checkout": "local", "blockers": [
                 {"goal": "reld-209", "when": "released",
                  "check": "gh release view --repo zackees/reld v0.9.0"}]},
            {"id": "4641", "repo": "FastLED/FastLED", "main": "master",
             "blockers": [{"goal": "4642", "when": "merged"}]},
        ],
        "proposed_goals": [],
        "risks": [],
    }


def invalid_plan(case: str) -> tuple[dict[str, Any], str]:
    plan = copy.deepcopy(run_plan())
    goals = plan["goals"]
    if case == "cycle":
        goals[2]["blockers"] = [{"goal": "4641", "when": "merged"}]
        reason = "dependency cycle: 4642 → 4641 → 4642"
    elif case == "unknown-goal":
        goals[2]["blockers"] = [{"goal": "reld-999", "when": "merged"}]
        reason = 'unknown goal "reld-999" in blockers of 4642'
    elif case == "missing-check":
        del goals[2]["blockers"][0]["check"]
        reason = "blocker reld-209 → 4642 (released) has no check"
    elif case == "unknown-condition":
        goals[2]["blockers"][0]["when"] = "shipped"
        reason = 'unknown condition "shipped"'
    elif case == "missing-repo":
        del goals[2]["repo"]
        reason = "goal 4642: repo and main are required"
    elif case == "missing-main":
        del goals[2]["main"]
        reason = "goal 4642: repo and main are required"
    else:
        assert case == "sister-local"
        goals[0]["checkout"] = "local"
        reason = "goal reld-208: a sister repo must use a worktree"
    return plan, reason


@pytest.mark.parametrize("case", [
    "cycle", "unknown-goal", "missing-check", "unknown-condition",
    "missing-repo", "missing-main", "sister-local",
])
def test_invalid_orchestration_stops_before_planning(harness: Harness, case: str) -> None:
    plan, reason = invalid_plan(case)
    _seed(harness, [101])
    _plan_phase(harness)
    script = _script(harness, [101], [_structured(_classify({101: ("bug", None)}, {}))])
    script["roles"][-1]["steps"][0]["tool_use"]["input"]["args"]["orchestration"] = plan
    before = harness.git("status", "--porcelain")
    result = harness.run(
        "Validate the supplied run plan before starting any role.", script, timeout=120,
    )
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    roles = {row["role"] for row in result.requests}
    assert roles == {"main"}, {"unexpected_roles": sorted(roles - {"main"})}
    # JSON encodes quotes inside the workflow error; decode them for diagnostics.
    assert reason in _told(result).replace('\\"', '"'), _told(result)[-4000:]
    assert harness.git("status", "--porcelain") == before
    assert harness.read_gh_state()["issues"]["101"]["state"] == "open"


def orchestrator_script(steps: list[dict[str, Any]]) -> dict[str, Any]:
    return {"default_text": "DONE", "roles": [
        {"name": "orchestrator", "match": "You are the /grind orchestrator.", "steps": steps},
        {"name": "main", "steps": [
            {"tool_use": {"name": "Agent", "input": {
                "subagent_type": "grind-orchestrator", "description": "Inspect the run graph",
                "prompt": "Inspect the supplied goals without changing files.",
                "run_in_background": False,
            }}},
            {"text": "INSPECTED"},
        ]},
    ]}


def test_orchestrator_router_call_is_readonly(harness: Harness) -> None:
    """T1/T3: native Agent may start the advisor, which cannot mutate files."""
    h = harness
    target = h.repo / "orchestrator-must-not-write"
    commands = [
        f"git -C {shlex.quote(str(h.repo))} switch -c forbidden-orchestrator",
        "cargo build",
        f"echo changed > {shlex.quote(str(target))}",
    ]
    denied = {"is_error": True, "content_contains": "grind-orchestrator"}
    steps = [_bash(commands[0])]
    steps.extend(_after(denied, _bash(command)) for command in commands[1:])
    steps.append(_after(denied, _bash("git status --porcelain")))
    steps.append(_after({"is_error": False}, {"text": "READONLY_CONFIRMED"}))
    before = h.git("status", "--porcelain")
    branch = h.git("branch", "--show-current")
    result = h.run("Call the read-only run orchestrator once.", orchestrator_script(steps))
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    request = result.first_request("orchestrator")
    assert not {"Write", "Edit", "MultiEdit", "NotebookEdit", "Agent"} & set(request["tools"])
    _no_notes(result)
    assert not target.exists()
    assert h.git("status", "--porcelain") == before
    assert h.git("branch", "--show-current") == branch
    assert h.git("branch", "--list", "forbidden-orchestrator") == ""


def test_orchestrator_exception_keeps_workers_internal(harness: Harness) -> None:
    """T2: adding the router advisor must not open direct worker delegation."""
    script = orchestrator_script([])
    call = script["roles"][-1]["steps"][0]["tool_use"]["input"]
    call["subagent_type"] = "grind-editor"
    script["roles"][-1]["steps"][1] = _after(
        {"is_error": True, "content_contains": "internal /grind role"}, {"text": "REFUSED"},
    )
    result = harness.run("Try the internal role only to verify its denial.", script)
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    _no_notes(result)
    assert {row["role"] for row in result.requests} == {"main"}


def test_orchestrator_reads_issues_and_files_from_both_repos(harness: Harness) -> None:
    """T4: the native advisor sees distinct same-number issues and dirty repos."""
    h = harness
    sister = h.root / "sister repo"
    sister.mkdir()
    h.git("init", "-q", "-b", "master", cwd=sister)
    repositories = {}
    for name, root in [("primary", h.repo), ("sister", sister)]:
        (root / "context.txt").write_text(f"ORCHESTRATOR_FILE_{name.upper()}\n")
        repositories[f"o/{name}"] = {
            "repo": f"o/{name}", "default_branch": "main" if name == "primary" else "master",
            "issues": {"7": {"title": name, "body": f"ORCHESTRATOR_ISSUE_{name.upper()}"}},
        }
    h.write_gh_state({"repo": "o/primary", "repositories": repositories})
    steps = []
    for name, root in [("primary", h.repo), ("sister", sister)]:
        steps.append(_bash(f"gh issue view 7 --repo o/{name} --json body"))
        steps.append(_after(
            {"is_error": False, "content_contains": f"ORCHESTRATOR_ISSUE_{name.upper()}"},
            {"tool_use": {"name": "Read", "input": {"file_path": str(root / "context.txt")}}},
        ))
        steps.append(_after(
            {"is_error": False, "content_contains": f"ORCHESTRATOR_FILE_{name.upper()}"},
            _bash(f"git -C {shlex.quote(str(root))} status --porcelain"),
        ))
    steps.append(_after({"is_error": False}, {"text": "BOTH_REPOSITORIES_INSPECTED"}))
    before = [h.git("status", "--porcelain", cwd=root) for root in (h.repo, sister)]
    result = h.run("Inspect both repositories without changing either.", orchestrator_script(steps))
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    _no_notes(result)
    seen = [row for row in result.requests if row["role"] == "orchestrator"]
    assert seen
    assert not any(block["is_error"] for row in seen for block in row["tool_results"])
    text = str([row["tool_results"] for row in seen])
    for marker in ["ISSUE_PRIMARY", "ISSUE_SISTER", "FILE_PRIMARY", "FILE_SISTER"]:
        assert f"ORCHESTRATOR_{marker}" in text, marker
    assert [h.git("status", "--porcelain", cwd=root) for root in (h.repo, sister)] == before


def test_valid_orchestration_allows_readonly_planning(harness: Harness) -> None:
    """A valid graph must still reach classification; rejection is selective."""
    _seed(harness, [101])
    _plan_phase(harness)
    script = _script(harness, [101], [_structured(_classify({101: ("bug", None)}, {}))])
    args = script["roles"][-1]["steps"][0]["tool_use"]["input"]["args"]
    args["orchestration"] = {
        "session_repo": "o/r",
        "session_root": str(harness.repo),
        "goals": [{"id": "101", "repo": "o/r", "main": "main", "blockers": []}],
        "proposed_goals": [], "risks": [],
    }
    before = harness.git("status", "--porcelain")
    result = harness.run("Validate the graph and classify without doing implementation.", script)
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    assert {row["role"] for row in result.requests} == {"main", "planner"}
    _no_notes(result)
    assert _path(result) == "simple", _told(result)[-3000:]
    assert harness.git("status", "--porcelain") == before


@pytest.mark.parametrize("mode", ["sequential", "parallel"])
@pytest.mark.parametrize("case", ["root", "main", "classification"])
def test_approved_context_mismatch_stops_before_agents(harness: Harness, mode: str, case: str):
    _seed(harness, [101])
    _plan_phase(harness)
    script = _script(harness, [101], [])
    args = script["roles"][-1]["steps"][0]["tool_use"]["input"]["args"]
    args["mode"] = mode
    graph = {"session_repo": "o/r", "session_root": str(harness.repo), "goals": [
        {"id": "101", "repo": "o/r", "main": "main", "blockers": []},
        {"id": "102", "repo": "o/r", "main": "main",
         "blockers": [{"goal": "101", "when": "merged"}]},
    ]}
    if case == "root":
        graph["session_root"] = str(harness.root / "another-checkout")
        reason = "execution root differs"
    elif case == "main":
        graph["goals"][0]["main"] = "master"
        reason = "execution main differs"
    else:
        args["plan"] = {"stages": [
            {"stage": "feature", "depends_on_bugs": {"101": ["102"]}},
        ]}
        reason = "classification dependency 102 → 101 is absent"
    args["orchestration"] = graph
    before = harness.git("status", "--porcelain")
    result = harness.run("Reject inconsistent approval before starting agents.", script)
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    assert {row["role"] for row in result.requests} == {"main"}
    assert reason in _told(result), _told(result)[-3000:]
    assert harness.git("status", "--porcelain") == before


@pytest.mark.parametrize("flag", ["--filters", "--filte"])
def test_orchestrator_cannot_execute_configured_git_filters(harness: Harness, flag: str) -> None:
    h = harness
    canary = h.root / "filter-was-executed"
    (h.repo / "filter-input").write_text("plain blob\n")
    (h.repo / ".gitattributes").write_text("filter-input filter=advisor-canary\n")
    h.git("add", "filter-input", ".gitattributes")
    h.git("commit", "-qm", "filter fixture")
    h.git("config", "filter.advisor-canary.smudge", f"touch {shlex.quote(str(canary))}")
    command = f"git -C {shlex.quote(str(h.repo))} cat-file {flag} HEAD:filter-input"
    steps = [_bash(command), _after(
        {"is_error": True, "content_contains": "grind-orchestrator"}, {"text": "FILTER_REFUSED"},
    )]
    before = h.git("status", "--porcelain")
    result = h.run("Inspect the filtered blob without executing configured commands.",
                   orchestrator_script(steps))
    assert result.returncode == 0, result.stderr + result.stdout[-3000:]
    _no_notes(result)
    assert "FILTER_REFUSED" in str(result.requests)
    assert not canary.exists()
    assert h.git("status", "--porcelain") == before

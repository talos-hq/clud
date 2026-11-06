"""`/grind`'s review gate, parking and retry rules on the real Claude Code.

#1424: the reviewer cannot run anything, so an unrun check is never a
rejection. It approves on reading and lists `must_verify`, which reaches the
integrator's prompt; a rejection that only says "nothing has been run" is
overridden. A rejected sequential goal's files are parked on a local
`wip/grind-<goal>` branch by an integrator `PARK goal` call, so the next goal
starts from a clean `origin/<main>`, and a goal that depends on a rejected
goal is blocked before any worker writes.

#1425: the integrator reads a deterministic failure instead of rerunning the
suite. A scripted model cannot decide, so the test pins what the integrator
is told, and that clud's hook refuses it a lint/test retry loop.

Script note (as in `test_grind.py`): a step's `expect` checks the tool
results of the *previous* step.
"""

from __future__ import annotations

import json
from typing import Any

from tests.harness.harness import Harness, RunResult
from tests.harness.test_grind import (
    _after,
    _bash,
    _by_role,
    _goal_roles,
    _goals,
    _merged,
    _no_notes,
    _role,
    _script,
    _structured,
)
from tests.harness.test_grind_scripts import _in_repo, _ran, _set_steps

OK = {"is_error": False}
MUST = "python -m pytest tests/test_gate.py -k must_verify_marker"
REAL_REJECTION = "The retry path drops the error; the change is wrong."
NOT_RUN = "Nothing has been run yet. My role can't run tests or lint, so I cannot confirm it."


def _review(roles: list[dict[str, Any]], goal: str, verdict: dict[str, Any]) -> None:
    _set_steps(roles, f"reviewer:{goal}", [_structured(verdict)])


def _park_role(h: Harness, goal: str) -> dict[str, Any]:
    """The integrator's park call for `goal`: its file onto wip/grind-<goal>.

    Listed before the goal's integrator role, which also matches "Goal <g>:".
    """
    repo = str(h.repo)
    park = (
        f"git -C {repo} switch -q -c wip/grind-{goal} && git -C {repo} add -- {goal}.txt && "
        f"git -C {repo} commit -q -m park-{goal} && git -C {repo} fetch -q origin main && "
        f"git -C {repo} switch -q --detach origin/main && git -C {repo} status --porcelain"
    )
    parked = {
        "parked": True,
        "branch": f"wip/grind-{goal}",
        "files": [f"{goal}.txt"],
        "clean": True,
        "summary": "parked",
    }
    return _role(
        f"park:{goal}",
        "integrator",
        [f"Goal {goal}:", f"PARK goal {goal}"],
        [_bash(park), _after(OK, _structured(parked))],
    )


def _told(result: RunResult) -> str:
    """What the main session was told: the workflow's result arrives there as
    a later task notification (as `test_grind.py`'s dead-planner test relies
    on), so text assertions on it come after the side-effect assertions."""
    main = json.dumps([r["messages"] for r in result.requests if r["role"] == "main"])
    return main + result.stdout


def _messages(result: RunResult, role: str) -> str:
    return json.dumps([r["messages"] for r in result.requests if r["role"] == role])


# ---- (a) must_verify reaches the integrator ------------------------------------------


def test_approved_review_hands_must_verify_to_the_integrator(harness: Harness) -> None:
    roles = _goal_roles(harness, "a")
    _review(roles, "a", {"approved": True, "summary": "reads right", "must_verify": [MUST]})
    result = harness.run(
        "start grind", _script(harness, "sequential", _goals("a"), roles), timeout=300
    )
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)
    assert _merged(harness) == ["grind/a"]
    told = result.prompt_text("integrator:a")
    assert MUST in told, told[-3000:]
    assert "must_verify" in told


def test_a_nothing_has_been_run_rejection_is_overridden(harness: Harness) -> None:
    roles = _goal_roles(harness, "a")
    _review(roles, "a", {"approved": False, "summary": NOT_RUN, "must_verify": [MUST]})
    result = harness.run(
        "start grind", _script(harness, "sequential", _goals("a"), roles), timeout=300
    )
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)
    assert _merged(harness) == ["grind/a"]
    told = result.prompt_text("integrator:a")
    assert MUST in told
    assert "the rejection was overridden" in told


# ---- (b) a rejected sequential goal is parked ----------------------------------------


def test_rejected_goal_is_parked_and_the_next_goal_starts_clean(harness: Harness) -> None:
    repo = str(harness.repo)
    roles = [_park_role(harness, "a"), *_goal_roles(harness, "a")]
    _review(roles, "a", {"approved": False, "summary": REAL_REJECTION})
    # Goal c's worker must not see a.txt: Read of it fails, then c.txt is written.
    write_c = {"file_path": f"{repo}/c.txt", "content": "c\n"}
    worker_c = [
        {"tool_use": {"name": "Read", "input": {"file_path": f"{repo}/a.txt"}}},
        _after({"is_error": True}, {"tool_use": {"name": "Write", "input": write_c}}),
        _after(OK, _structured({"files_touched": ["c.txt"], "summary": "wrote it"})),
    ]
    roles += _goal_roles(harness, "c", worker_steps=worker_c)
    result = harness.run(
        "start grind", _script(harness, "sequential", _goals("a", "c"), roles), timeout=300
    )
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)
    ran = _by_role(result)
    assert "integrator:a" not in ran
    assert "park:a" in ran
    assert max(ran["park:a"]) < min(ran["planner:c"])
    assert _merged(harness) == ["grind/c"]

    park = _messages(result, "park:a")
    assert "Park branch: wip/grind-a" in park
    assert "Goal files: a.txt" in park

    # The checkout is clean and a.txt lives only on the local park branch.
    assert not (harness.repo / "a.txt").exists()
    status = harness.git("status", "--porcelain")
    assert [line for line in status.splitlines() if ".claude" not in line] == [], status
    assert harness.git("show", "wip/grind-a:a.txt") == "a"
    assert harness.git("ls-remote", "--heads", "origin", "wip/grind-a") == ""
    assert "a.txt" not in harness.git("ls-tree", "-r", "--name-only", "grind/c").split()

    told = _told(result)
    assert "review rejected" in told
    assert "parked on wip/grind-a" in told


# ---- (c) a dependent of a rejected goal is blocked -----------------------------------


def test_dependent_of_a_rejected_goal_is_blocked_in_sequential_mode(harness: Harness) -> None:
    roles = [_park_role(harness, "a"), *_goal_roles(harness, "a")]
    _review(roles, "a", {"approved": False, "summary": REAL_REJECTION})
    roles += _goal_roles(harness, "b", depends_on=["a"])
    result = harness.run(
        "start grind", _script(harness, "sequential", _goals("a", "b"), roles), timeout=300
    )
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)
    ran = _by_role(result)
    assert "planner:b" in ran
    for role in ("worker:b", "reviewer:b", "integrator:b", "integrator:a"):
        assert role not in ran, role
    assert _merged(harness) == []
    assert "blocked: dependency a was rejected" in _told(result)


def test_dependent_of_a_rejected_goal_is_never_integrated_in_parallel(harness: Harness) -> None:
    roles = _goal_roles(harness, "a")
    _review(roles, "a", {"approved": False, "summary": REAL_REJECTION})
    roles += _goal_roles(harness, "b", depends_on=["a"])
    result = harness.run(
        "start grind", _script(harness, "parallel", _goals("a", "b"), roles), timeout=300
    )
    assert result.returncode == 0, result.stdout[-3000:]
    ran = _by_role(result)
    assert "integrator:a" not in ran  # no integrator call at all: parallel never parks
    assert "integrator:b" not in ran
    assert _merged(harness) == []
    assert "blocked: dependency a was rejected" in _told(result)


# ---- #1425: read a deterministic failure, never loop the suite ------------------------


def _failing_test_script(h: Harness) -> None:
    log = h.logs / "ran.jsonl"
    body = (
        "#!/bin/bash\n"
        f"printf '{{\"name\":\"test\",\"rc\":1}}\\n' >> \"{log}\"\n"
        'echo "FAILED tests/test_widget.py::test_widget - AssertionError: 2 != 3"\n'
        "exit 1\n"
    )
    (h.repo / "test").write_text(body, encoding="utf-8")
    (h.repo / "test").chmod(0o755)
    h.git("add", "test")
    h.git("commit", "-q", "-m", "add failing test script")
    h.git("push", "-q", "origin", "main")


def test_integrator_reads_a_deterministic_failure_instead_of_looping(harness: Harness) -> None:
    _failing_test_script(harness)
    failure = "FAILED tests/test_widget.py::test_widget"
    gave_up = {
        "pushed": False,
        "summary": "test_widget fails deterministically; not an infrastructure error",
        "failure_log": f"bash ./test\n{failure} - AssertionError: 2 != 3",
    }
    integrator = [
        _bash(_in_repo(harness, "for i in 1 2 3; do bash ./test; done")),
        _after(
            {"is_error": True, "content_contains": "#1425"},
            _bash(_in_repo(harness, "bash ./test")),
        ),
        _after({"is_error": True, "content_contains": failure}, _structured(gave_up)),
    ]
    roles = [_park_role(harness, "g"), *_goal_roles(harness, "g")]
    _set_steps(roles, "integrator:g", integrator)
    script = _script(harness, "sequential", _goals("g"), roles, scripts={"test": "bash ./test"})
    result = harness.run("start grind", script, timeout=300)
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)

    # The retry loop was refused, so the suite ran once.
    assert len(_ran(harness)) == 1, _ran(harness)
    assert _merged(harness) == []

    # What the integrator is told (a scripted model can't decide; pin the text).
    system = " ".join(result.first_request("integrator:g")["system"].split())
    for rule in (
        "Read before retrying.",
        "`SESSION relay closed before Exit`",
        "at most twice",
        "Same failure twice is real.",
        "fails on untouched `origin/<main>`",
        "Wait by condition.",
    ):
        assert rule in system, rule
    assert "MULTI-PLATFORM LINT GATE" in result.prompt_text("integrator:g")

    told = _told(result)
    assert "test_widget fails deterministically" in told
    assert "parked on wip/grind-g" in told

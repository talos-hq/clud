"""`grind-run`'s bug stage, then feature stage, on the real Claude Code (#1409).

When the router passes `plan` (the `grind-plan/v1` object from the `/grind`
skill, section 3b), the workflow runs the `bugs` stage first, each goal based
on `origin/<main>`, and plans no feature goal until every bug goal has
integrated. Feature goals are based on the feature stage's branch
(`origin/grind/meta-100-1f3a`). A stuck bug (`rules.stuck_bug:
block_dependents_only`) blocks only the feature goals that list it in
`depends_on_bugs`; the rest still run. Without `plan` the old single-stage
order, based on `origin/main`, is kept.

These tests hand one `grind-run` call the goals of both stages, the
fallback form. The router's one-call-per-stage flow (T1-T3, T5 across
calls, T6, H7) is in `test_grind_tracks.py`.

Script note: a step's `expect` checks the tool results of the *previous*
step, so an expectation about a command sits on the step after it.
"""

from __future__ import annotations

import json
from typing import Any

from tests.harness.harness import Harness, RunResult
from tests.harness.worlds import _issue, _world

MARK = {
    "prework": "You are the /grind prework role",
    "planner": "You are the /grind planner",
    "worker": "You are the /grind editor",
    "reviewer": "You are the /grind reviewer",
    "integrator": "You are the /grind integrator",
    "lander": "You are the /grind lander",
    "chaperone": "You are the /grind chaperone",
}
META = "100"
RUN_ID = "1f3a"
FEATURE = f"grind/meta-{META}-{RUN_ID}"
BUGS = ["101", "103"]
FEATURES = ["102", "104"]
MARKER = "<!-- grind:v1 plan run="
# fake_gh numbers comments from `next_id` (1000 in every world here).
PLAN_URL = f"https://github.com/o/r/issues/{META}#issuecomment-1000"
OK = {"is_error": False}


def _branch(goal: str) -> str:
    """A goal branch whose last path part is not a number: fake_gh reads a
    numeric tail (`grind/103`) as PR #103, not as the head branch."""
    return f"grind/goal-{goal}"


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


# ---- world ---------------------------------------------------------------------


def _seed(h: Harness) -> None:
    """Meta #100 with bugs 101, 103 and features 102, 104 as sub-issues."""
    issues = {int(META): _issue("meta: backlog", "Tracked as sub-issues.")}
    for n in (101, 102, 103, 104):
        issues[n] = _issue(f"child {n}", f"do {n}", parent=int(META))
    h.write_gh_state(_world(issues))
    # One call carrying both stages: the feature branch already exists. (The
    # router normally makes one call per stage; see test_grind_tracks.py.)
    h.git("push", "-q", "origin", f"main:{FEATURE}")


def _run_facts(h: Harness) -> None:
    run = h.run_facts_path()
    run.parent.mkdir(parents=True, exist_ok=True)
    run.write_text(json.dumps({"mode": "sequential", "meta": META}), encoding="utf-8")


def _plan() -> dict[str, Any]:
    """The `/grind` skill's section 3b example, trimmed to these four goals."""
    return {
        "schema": "grind-plan/v1",
        "run_id": RUN_ID,
        "meta": int(META),
        "original": 95,
        "repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "preflight": {"action": "none", "branch": "main"},
        "structure": "simple",
        "stages": [
            {"stage": "bugs", "base": "main", "children": [101, 103]},
            {
                "stage": "feature",
                "group": "auth rework",
                "sub_meta": None,
                "branch": FEATURE,
                "base": FEATURE,
                "children": [102, 104],
                "depends_on_bugs": {"104": [103]},
            },
        ],
        "deferred_groups": [],
        "feature_merge": "auto",
        "problem_reporting": "issue",
        "models": {},
        "ci": False,
        "scripts": {},
        "rules": {"stuck_bug": "block_dependents_only", "no_overlap": "bugs_only"},
    }


# ---- scripted roles ------------------------------------------------------------


def _prework() -> dict[str, Any]:
    steps = [
        _bash(f"gh issue comment {META} --repo o/r --body '{MARKER}{RUN_ID} -->'"),
        _after(OK, _structured({"posted": True, "plan_url": PLAN_URL, "part_urls": [PLAN_URL]})),
    ]
    return _role("prework", "prework", "/grind-prework", steps)


def _goal_roles(
    h: Harness, goal: str, *, lander: list[dict[str, Any]] | None = None
) -> list[dict[str, Any]]:
    """One goal's planner, worker, reviewer, integrator and lander."""
    branch = _branch(goal)
    repo = str(h.repo)
    at = f"Goal {goal}:"
    plan = {
        "checkout": repo,
        "branch": branch,
        "depends_on": [],
        "verify": "true",
        "tasks": [{"id": "t1", "files": [f"{goal}.txt"], "instructions": f"write {goal}.txt"}],
    }
    write = {"file_path": f"{repo}/{goal}.txt", "content": f"{goal}\n"}
    push = (
        f"git -C {repo} switch -q main && git -C {repo} switch -q -c {branch} && "
        f"git -C {repo} add {goal}.txt && git -C {repo} commit -q -m {goal} && "
        f"git -C {repo} push -q -u origin {branch} && gh pr create --title {goal} --head {branch}"
    )
    url = f"https://github.com/o/r/pull/{branch}"
    land = lander or [
        _bash(f"gh pr merge {branch} --squash --match-head-commit HEAD"),
        _after(OK, _structured({"verdict": "merged", "summary": "green"})),
    ]
    return [
        _role(f"planner:{goal}", "planner", at, [_structured(plan)]),
        _role(
            f"worker:{goal}",
            "worker",
            at,
            [
                {"tool_use": {"name": "Write", "input": write}},
                _after(OK, _structured({"files_touched": [f"{goal}.txt"], "summary": "wrote"})),
            ],
        ),
        _role(
            f"reviewer:{goal}", "reviewer", at, [_structured({"approved": True, "summary": "ok"})]
        ),
        _role(
            f"integrator:{goal}",
            "integrator",
            at,
            [_bash(push), _after(OK, _structured({"pushed": True, "pr_url": url, "summary": "p"}))],
        ),
        _role(f"chaperone:{goal}", "chaperone", [at, "Follow your WATCH"], land),
    ]


def _script(
    h: Harness, roles: list[dict[str, Any]], *, plan: dict[str, Any] | None
) -> dict[str, Any]:
    # Interleaved on purpose: without stages, sequential mode would plan 102
    # before 103 integrates, so the stage-order test fails on the old code.
    goals = ["101", "102", "103", "104"]
    args: dict[str, Any] = {
        "repo": str(h.repo),
        "github_repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "goals": [{"id": g, "title": f"child {g}", "brief": f"do {g}"} for g in goals],
    }
    if plan is not None:
        args["meta"] = META
        args["plan"] = plan
    main = {
        "name": "main",
        "steps": [
            {"tool_use": {"name": "Workflow", "input": {"name": "grind-run", "args": args}}},
            {"text": "GRIND_DONE"},
        ],
    }
    return {"default_text": "OK", "roles": [*roles, main]}


# ---- helpers -------------------------------------------------------------------


def _text(value: Any) -> str:
    """Every string inside `value`, joined: a request's raw prompt text."""
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        return "\n".join(_text(v) for v in value.values())
    if isinstance(value, list):
        return "\n".join(_text(v) for v in value)
    return ""


def _prompt(result: RunResult, role: str) -> str:
    return _text(result.first_request(role).get("messages"))


def _first(result: RunResult) -> dict[str, int]:
    """Each scripted role's first request number, i.e. the order agents started."""
    seen: dict[str, int] = {}
    for r in result.requests:
        seen[r["role"]] = min(seen.get(r["role"], r["n"]), r["n"])
    return seen


def _merged(h: Harness) -> list[str]:
    prs = h.read_gh_state().get("prs", [])
    return sorted(p["head"] for p in prs if p.get("state") == "MERGED")


def _no_notes(result: RunResult) -> None:
    notes = [(r["role"], r["note"]) for r in result.requests if r.get("note")]
    assert not notes, notes


def _told(result: RunResult) -> str:
    seen = [r["messages"] for r in result.requests if r["role"] == "main"]
    return json.dumps(seen, ensure_ascii=False) + result.stdout


def _all_roles(h: Harness, **landers: list[dict[str, Any]]) -> list[dict[str, Any]]:
    roles: list[dict[str, Any]] = []
    for g in [*BUGS, *FEATURES]:
        roles += _goal_roles(h, g, lander=landers.get(g))
    return roles


def _run(
    h: Harness, roles: list[dict[str, Any]], *, plan: dict[str, Any] | None
) -> RunResult:
    _seed(h)
    _run_facts(h)
    if plan is not None:
        roles = [_prework(), *roles]
    result = h.run("start grind", _script(h, roles, plan=plan), timeout=600)
    assert result.returncode == 0, result.stdout[-3000:]
    return result


# ---- tests ---------------------------------------------------------------------


def test_bug_stage_integrates_before_any_feature_goal_is_planned(harness: Harness) -> None:
    result = _run(harness, _all_roles(harness), plan=_plan())
    _no_notes(result)
    first = _first(result)
    for bug in BUGS:
        for feature in FEATURES:
            assert first[f"integrator:{bug}"] < first[f"planner:{feature}"], (bug, feature, first)
    assert _merged(harness) == [_branch(g) for g in ("101", "102", "103", "104")]


def test_feature_integrator_uses_the_feature_branch_base(harness: Harness) -> None:
    result = _run(harness, _all_roles(harness), plan=_plan())
    _no_notes(result)
    feature = _prompt(result, "integrator:102")
    assert f"Base: origin/{FEATURE}" in feature, feature[-3000:]
    assert "Base: origin/main" not in feature
    bug = _prompt(result, "integrator:101")
    assert "Base: origin/main" in bug, bug[-3000:]
    assert f"Base: origin/{FEATURE}" not in bug


def test_stuck_bug_blocks_only_its_dependents(harness: Harness) -> None:
    gave_up = [_structured({"verdict": "stuck", "summary": "stuck", "failure_log": "red"})]
    result = _run(harness, _all_roles(harness, **{"103": gave_up}), plan=_plan())
    merged = _merged(harness)
    assert _branch("103") not in merged
    assert _branch("104") not in merged
    assert _branch("102") in merged, merged
    assert _branch("101") in merged, merged
    ran = _first(result)
    assert "planner:104" not in ran, ran
    assert "integrator:104" not in ran, ran
    told = _told(result)
    assert "blocked: bug #103" in told, told[-3000:]


def test_no_plan_keeps_old_order(harness: Harness) -> None:
    result = _run(harness, _all_roles(harness), plan=None)
    _no_notes(result)
    assert "prework" not in _first(result)
    for g in [*BUGS, *FEATURES]:
        text = _prompt(result, f"integrator:{g}")
        assert "Base: origin/main" in text, (g, text[-3000:])
        assert FEATURE not in text
    assert _merged(harness) == [_branch(g) for g in ("101", "102", "103", "104")]

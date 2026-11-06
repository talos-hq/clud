"""No overlap between feature PRs, on the real Claude Code (#1412, #1392 O1-O4).

With `rules.no_overlap: bugs_only`, an open feature PR under the same top meta
(an open PR into `main` with head `grind/meta-<X>-*`, X the top meta or one
of its sub-metas) makes the router set `plan.waiting_on_pr`, and
`grind-run` then runs only the bug stage: every feature goal is reported
`deferred` with a note naming the PR, and the run logs `no overlap`. Once that
PR is merged, or when the open feature PR belongs to a different top meta, the
next plan has no `waiting_on_pr` and its feature stage runs.

`_waiting_on` mirrors the router's rule, so each test builds its plan the way
`/grind` would from the fake GitHub state.

Script note: a step's `expect` checks the tool results of the *previous*
step, so an expectation about a command sits on the step after it.
"""

from __future__ import annotations

import json
import re
from pathlib import Path
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
}
META = "100"
RUN_ID = "1f3a"
FEATURE = f"grind/meta-{META}-{RUN_ID}"
BUGS = ["101", "103"]
FEATURES = ["102", "104"]
# Goals of the second run in O3, planned after the feature PR merged.
BUG2 = "105"
FEATURE2 = "106"
FEATURE_PR = 90
MARKER = "<!-- grind:v1 plan run="
PLAN_URL = f"https://github.com/o/r/issues/{META}#issuecomment-1000"
OK = {"is_error": False}


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


def _feature_pr(head: str, state: str) -> dict[str, Any]:
    return {
        "number": FEATURE_PR,
        "head": head,
        "state": state,
        "title": f"feature {head}",
        "base": "main",
        "draft": state == "OPEN",
        "body": f"Refs #{META}",
    }


def _seed(h: Harness, prs: list[dict[str, Any]]) -> None:
    """Meta #100 with children 101-106 as sub-issues, plus `prs`."""
    issues = {int(META): _issue("meta: backlog", "Tracked as sub-issues.")}
    for n in (101, 102, 103, 104, 105, 106):
        issues[n] = _issue(f"child {n}", f"do {n}", parent=int(META))
    world = _world(issues)
    world["prs"] = prs
    h.write_gh_state(world)
    h.git("push", "-q", "origin", f"main:{FEATURE}")
    run = h.run_facts_path()
    run.parent.mkdir(parents=True, exist_ok=True)
    run.write_text(json.dumps({"mode": "sequential", "meta": META}), encoding="utf-8")


def _waiting_in(state: dict[str, Any], meta: str) -> int | None:
    """The router's rule (skill section 1b): an open PR into `main` whose head
    is `grind/meta-<X>-…`, where X is the top meta or one of its sub-issues
    (a meta of metas names each feature branch after its sub-meta)."""
    top = state["issues"].get(str(meta), {})
    owners = {str(meta), *(str(s["number"]) for s in top.get("sub_issues", []))}
    for p in state.get("prs", []):
        head = str(p.get("head", ""))
        if p.get("state") != "OPEN" or p.get("base", "main") != "main":
            continue
        owner = head.removeprefix("grind/meta-").split("-", 1)[0]
        if head.startswith("grind/meta-") and owner in owners:
            return int(p["number"])
    return None


def _waiting_on(h: Harness, meta: str) -> int | None:
    return _waiting_in(h.read_gh_state(), meta)


def _branch(goal: str) -> str:
    """`grind/goal-<id>`, not `grind/<id>`: fake_gh reads a target whose last
    path segment is a number as a PR number."""
    return f"grind/goal-{goal}"


def _plan(
    bugs: list[str], features: list[str], *, waiting: int | None, merge: str = "later"
) -> dict[str, Any]:
    plan: dict[str, Any] = {
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
            {"stage": "bugs", "base": "main", "children": [int(b) for b in bugs]},
            {
                "stage": "feature",
                "group": "auth rework",
                "sub_meta": None,
                "branch": FEATURE,
                "base": FEATURE,
                "children": [int(f) for f in features],
                "depends_on_bugs": {},
            },
        ],
        "deferred_groups": [],
        "feature_merge": merge,
        "problem_reporting": "issue",
        "models": {},
        "ci": False,
        "scripts": {},
        "rules": {"stuck_bug": "block_dependents_only", "no_overlap": "bugs_only"},
    }
    if waiting is not None:
        plan["waiting_on_pr"] = waiting
    return plan


# ---- scripted roles ------------------------------------------------------------


def _prework() -> dict[str, Any]:
    steps = [
        _bash(f"gh issue comment {META} --repo o/r --body '{MARKER}{RUN_ID} -->'"),
        _after(OK, _structured({"posted": True, "plan_url": PLAN_URL, "part_urls": [PLAN_URL]})),
    ]
    return _role("prework", "prework", "/grind-prework", steps)


def _goal_roles(h: Harness, goal: str) -> list[dict[str, Any]]:
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
    land = [
        _bash(f"gh pr merge {branch} --admin --squash"),
        _after(OK, _structured({"status": "merged", "summary": "green"})),
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
        _role(f"lander:{goal}", "lander", [at, "Fix rounds used: 0 of"], land),
    ]


def _feature_lander() -> dict[str, Any]:
    """Lands the feature PR itself under `feature_merge: auto`."""
    steps = [
        # One GitHub mutation per command (#1979).
        _bash(f"gh pr ready {FEATURE_PR}"),
        _after(OK, _bash(f"gh pr merge {FEATURE_PR} --merge")),
        _after(OK, _structured({"status": "merged", "summary": "feature merged"})),
    ]
    return _role("lander:feature", "lander", f"Feature PR: {FEATURE_PR}", steps)


def _script(
    h: Harness,
    roles: list[dict[str, Any]],
    goals: list[str],
    plan: dict[str, Any],
    feature: dict[str, Any] | None = None,
) -> dict[str, Any]:
    args: dict[str, Any] = {
        "repo": str(h.repo),
        "github_repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "goals": [{"id": g, "title": f"child {g}", "brief": f"do {g}"} for g in goals],
        "meta": META,
        "plan": plan,
    }
    if feature is not None:
        args["feature"] = feature
    main = {
        "name": "main",
        "steps": [
            {"tool_use": {"name": "Workflow", "input": {"name": "grind-run", "args": args}}},
            {"text": "GRIND_DONE"},
        ],
    }
    return {"default_text": "OK", "roles": [_prework(), *roles, main]}


# ---- helpers -------------------------------------------------------------------


def _first(result: RunResult) -> dict[str, int]:
    """Each scripted role's first request number, i.e. the order agents started."""
    seen: dict[str, int] = {}
    for r in result.requests:
        seen[r["role"]] = min(seen.get(r["role"], r["n"]), r["n"])
    return seen


def _merged(h: Harness) -> list[str]:
    prs = h.read_gh_state().get("prs", [])
    return sorted(p["head"] for p in prs if p.get("state") == "MERGED")


def _told(result: RunResult) -> str:
    seen = [r["messages"] for r in result.requests if r["role"] == "main"]
    return json.dumps(seen, ensure_ascii=False) + result.stdout


def _roles(h: Harness, goals: list[str]) -> list[dict[str, Any]]:
    roles: list[dict[str, Any]] = []
    for g in goals:
        roles += _goal_roles(h, g)
    return roles


def _run(h: Harness, script: dict[str, Any]) -> RunResult:
    result = h.run("start grind", script, timeout=600)
    assert result.returncode == 0, result.stdout[-3000:]
    return result


def _feature_stage_ran(h: Harness, result: RunResult, features: list[str]) -> None:
    ran = _first(result)
    merged = _merged(h)
    for f in features:
        assert f"planner:{f}" in ran, (f, ran)
        assert _branch(f) in merged, (f, merged)
    # The workflow source also contains the phrase; match only a rendered note.
    assert not re.search(r"waiting on feature PR #\d", _told(result))


# ---- tests ---------------------------------------------------------------------


def _bugs_only_ran(h: Harness, result: RunResult) -> None:
    """Bugs landed; every feature goal came back `deferred`, naming the PR."""
    ran = _first(result)
    merged = _merged(h)
    for bug in BUGS:
        assert _branch(bug) in merged, merged
    for f in FEATURES:
        assert f"planner:{f}" not in ran, (f, ran)
        assert _branch(f) not in merged, merged
    told = _told(result)
    assert f"no overlap: waiting on feature PR #{FEATURE_PR}" in told, told[-3000:]
    for f in FEATURES:
        # The workflow's summary entry for the goal, JSON-escaped in the log.
        pattern = rf'goal\\*"\s*:\s*\\*"{f}\\*".{{0,300}}?status\\*"\s*:\s*\\*"deferred'
        assert re.search(pattern, told, re.DOTALL), (f, told[-3000:])


def test_o1_open_feature_pr_runs_bugs_only(harness: Harness) -> None:
    _seed(harness, [_feature_pr(f"grind/meta-{META}-x", "OPEN")])
    waiting = _waiting_on(harness, META)
    assert waiting == FEATURE_PR
    plan = _plan(BUGS, FEATURES, waiting=waiting)
    goals = ["101", "102", "103", "104"]
    result = _run(harness, _script(harness, _roles(harness, goals), goals, plan))
    _bugs_only_ran(harness, result)


def test_o1_open_sub_meta_feature_pr_runs_bugs_only(harness: Harness) -> None:
    """Meta of metas: the waiting PR's branch is named after sub-meta #110 of
    #100, and the bugs-only plan (skill section 1b) moves the feature group
    out of `stages` into `deferred_groups`; its goals still never run."""
    sub = 110
    _seed(harness, [_feature_pr(f"grind/meta-{sub}-x", "OPEN")])
    state = harness.read_gh_state()
    state["issues"][str(sub)] = _issue(
        "grind: auth rework", "<!-- grind:v1 -->", labels=["grind:meta"], parent=int(META)
    )
    state["issues"][META]["sub_issues"].append({"number": sub, "state": "open"})
    harness.write_gh_state(state)
    waiting = _waiting_on(harness, META)
    assert waiting == FEATURE_PR
    plan = _plan(BUGS, FEATURES, waiting=waiting)
    group = plan["stages"].pop()
    plan["deferred_groups"] = [
        {"group": group["group"], "sub_meta": sub, "children": group["children"]}
    ]
    goals = ["101", "102", "103", "104"]
    result = _run(harness, _script(harness, _roles(harness, goals), goals, plan))
    _bugs_only_ran(harness, result)


def test_o_rule_scope_is_the_top_meta_and_its_sub_metas() -> None:
    """The router's no-overlap rule, on its own: which open PRs block #100."""
    issues = {int(META): _issue("meta", ""), 110: _issue("sub", "", parent=int(META))}
    issues[200] = _issue("other meta", "")
    world = _world(issues)

    def waiting(head: str, *, state: str = "OPEN", base: str = "main") -> int | None:
        pr = {**_feature_pr(head, state), "base": base}
        return _waiting_in({**world, "prs": [pr]}, META)

    assert waiting(f"grind/meta-{META}-1f3a") == FEATURE_PR
    assert waiting("grind/meta-110-1f3a") == FEATURE_PR
    assert waiting("grind/meta-200-1f3a") is None
    assert waiting(f"grind/meta-{META}-1f3a", state="MERGED") is None
    assert waiting(f"grind/meta-{META}-1f3a", state="CLOSED") is None
    # A goal PR into the feature branch is not a feature PR.
    assert waiting(f"grind/meta-{META}-1f3a-goal", base=f"grind/meta-{META}-1f3a") is None
    assert waiting(f"grind/meta-{META}0-1f3a") is None


def test_o2_merged_feature_pr_lets_the_feature_stage_run(harness: Harness) -> None:
    _seed(harness, [_feature_pr(f"grind/meta-{META}-x", "MERGED")])
    waiting = _waiting_on(harness, META)
    assert waiting is None
    plan = _plan(BUGS, FEATURES, waiting=waiting)
    assert "waiting_on_pr" not in plan
    goals = ["101", "102", "103", "104"]
    result = _run(harness, _script(harness, _roles(harness, goals), goals, plan))
    _feature_stage_ran(harness, result, FEATURES)


def test_o3_feature_pr_merged_in_run_unblocks_the_next_plan(harness: Harness) -> None:
    _seed(harness, [_feature_pr(FEATURE, "OPEN")])
    # This run owns the open feature PR, so its plan is not waiting on it.
    plan = _plan(BUGS, FEATURES, waiting=None, merge="auto")
    feature = {
        "branch": FEATURE,
        "worktree": str(Path(harness.repo) / ".clud" / "grind" / "worktrees" / "feature"),
        "pr": str(FEATURE_PR),
    }
    goals = ["101", "102", "103", "104"]
    roles = [*_roles(harness, goals), _feature_lander()]
    # The router records the feature in the run facts before the feature-stage
    # call (/grind 4b step 5); the hook reads the feature PR from there.
    run = harness.run_facts_path()
    run.parent.mkdir(parents=True, exist_ok=True)
    facts = {"mode": "sequential", "meta": META, "feature": feature, "feature_merge": "auto"}
    run.write_text(json.dumps(facts), encoding="utf-8")
    first = _run(harness, _script(harness, roles, goals, plan, feature))
    _feature_stage_ran(harness, first, FEATURES)
    assert FEATURE in _merged(harness), _merged(harness)

    waiting = _waiting_on(harness, META)
    assert waiting is None
    second_plan = _plan([BUG2], [FEATURE2], waiting=waiting)
    assert "waiting_on_pr" not in second_plan
    goals2 = [BUG2, FEATURE2]
    # The next `/grind` is a new session; its router records the same facts.
    harness.new_session()
    harness.write_run_facts(facts)
    second = _run(harness, _script(harness, _roles(harness, goals2), goals2, second_plan))
    _feature_stage_ran(harness, second, [FEATURE2])


def test_o4_feature_pr_under_another_meta_does_not_block(harness: Harness) -> None:
    _seed(harness, [_feature_pr("grind/meta-200-x", "OPEN")])
    waiting = _waiting_on(harness, META)
    assert waiting is None
    plan = _plan(BUGS, FEATURES, waiting=waiting)
    goals = ["101", "102", "103", "104"]
    result = _run(harness, _script(harness, _roles(harness, goals), goals, plan))
    _feature_stage_ran(harness, result, FEATURES)
    assert not re.search(r"no overlap: waiting on feature PR #\d", _told(result))

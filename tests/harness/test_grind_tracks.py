"""The bug stage, then the feature stage, one `grind-run` call each (#1409).

Covers #1392 §3 "Schedule" and cases T1, T2, T3, T5 (across calls), T6 and
H7 on the real Claude Code. The router (the scripted main session) starts
`grind-run` once per stage: the bug-stage call gets the plan and the bug
children, and its prework posts the plan. Only after that call's result
arrives does the router cut `grind/meta-100-1f3a` from the updated
`origin/main` and open the draft feature PR; then the feature-stage call gets
the feature children, `base`, `plan_url`, `feature`, `feature_merge` and
`stuck_bugs`. See `docs/architecture/grind.md`, "Bug stage, then feature
stage", and DD-100.

Harness facts these scripts rely on:

- The Workflow tool returns "launched in background" at once; its result
  reaches the main session later as a task notification, which starts a new
  main turn. So the main script ends its turn with text after each launch,
  and its next step runs when the notification arrives.
- `fake_gh pr merge` records the merge but moves no git ref. A bug
  integrator therefore also pushes its commit to `main`, standing in for the
  merge, so the feature branch cut afterwards can be checked for it.
- `fake_gh` reads a numeric last path part (`grind/103`) as a PR number, so
  goal branches are `grind/goal-<n>`.

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


def _branch(goal: str) -> str:
    return f"grind/goal-{goal}"


def _workflow(args: dict[str, Any]) -> dict[str, Any]:
    return {"tool_use": {"name": "Workflow", "input": {"name": "grind-run", "args": args}}}


# ---- world ---------------------------------------------------------------------


def _seed(h: Harness, children: list[str]) -> dict[str, Any]:
    """Meta #100 with `children` as native sub-issues; returns the meta issue."""
    issues = {int(META): _issue("meta: backlog", "Tracked as sub-issues.")}
    for n in children:
        issues[int(n)] = _issue(f"child {n}", f"do {n}", parent=int(META))
    h.write_gh_state(_world(issues))
    run = h.run_facts_path()
    run.parent.mkdir(parents=True, exist_ok=True)
    # The bug stage runs without `feature` in the run facts (plain lander caps).
    facts = {"mode": "sequential", "meta": META, "problem_reporting": "issue"}
    run.write_text(json.dumps(facts), encoding="utf-8")
    exclude = h.repo / ".git" / "info" / "exclude"
    exclude.write_text(exclude.read_text(encoding="utf-8") + ".clud/\n", encoding="utf-8")
    return h.read_gh_state()["issues"][META]


def _plan(bugs: list[str], features: list[str], depends: dict[str, list[str]]) -> dict[str, Any]:
    stages: list[dict[str, Any]] = []
    if bugs:
        stages.append({"stage": "bugs", "base": "main", "children": [int(b) for b in bugs]})
    if features:
        stages.append(
            {
                "stage": "feature",
                "group": "auth rework",
                "sub_meta": None,
                "branch": FEATURE,
                "base": FEATURE,
                "children": [int(f) for f in features],
                "depends_on_bugs": depends,
            }
        )
    return {
        "schema": "grind-plan/v1",
        "run_id": RUN_ID,
        "meta": int(META),
        "repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "preflight": {"action": "none", "branch": "main"},
        "structure": "simple",
        "stages": stages,
        "deferred_groups": [],
        "feature_merge": "later" if features else None,
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


def _goal(h: Harness, goal: str, *, feature: bool, stuck: bool = False) -> list[dict[str, Any]]:
    """One goal's planner, worker, reviewer, integrator and lander."""
    repo = str(h.repo)
    branch = _branch(goal)
    at = f"Goal {goal}:"
    base = FEATURE if feature else "main"
    plan = {
        "checkout": repo,
        "branch": branch,
        "depends_on": [],
        "verify": "true",
        "tasks": [{"id": "t1", "files": [f"{goal}.txt"], "instructions": f"write {goal}.txt"}],
    }
    write = {"file_path": f"{repo}/{goal}.txt", "content": f"{goal}\n"}
    git = f"git -C {repo}"
    kind, keyword = ("feat", "Refs") if feature else ("fix", "Closes")
    push = [
        f"{git} fetch -q origin",
        f"{git} switch -q -c {branch} origin/{base}",
        f"{git} add {goal}.txt",
        f"{git} commit -q -m '{kind}: #{goal}'",
        f"{git} push -q -u origin {branch}",
    ]
    if not feature and not stuck:
        # Stands in for the merge, which fake_gh only records.
        push.append(f"{git} push -q origin {branch}:main")
    push.append(
        f"gh pr create --title '{kind} #{goal}' --head {branch} --base {base} "
        f"--body '{keyword} #{goal}'"
    )
    url = f"https://github.com/o/r/pull/{branch}"
    land = (
        [_structured({"verdict": "stuck", "summary": "stuck", "failure_log": "red"})]
        if stuck
        else [
            _bash(f"gh pr merge {branch} --merge --match-head-commit HEAD"),
            _after(OK, _structured({"verdict": "merged", "summary": "green"})),
        ]
    )
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
            [
                _bash(" && ".join(push)),
                _after(OK, _structured({"pushed": True, "pr_url": url, "summary": "p"})),
            ],
        ),
        _role(f"chaperone:{goal}", "chaperone", [at, "Follow your WATCH"], land),
    ]


def _goals(ids: list[str]) -> list[dict[str, Any]]:
    return [{"id": g, "title": f"child {g}", "brief": f"do {g}"} for g in ids]


def _main(
    h: Harness,
    plan: dict[str, Any],
    bugs: list[str],
    features: list[str],
    stuck: list[str],
) -> dict[str, Any]:
    """The router: the bug-stage call, then (with features) setup and the
    feature-stage call. Each launch ends the turn; the next step runs when
    that call's notification arrives."""
    repo = str(h.repo)
    common = {"repo": repo, "main": "main", "mode": "sequential", "meta": META, "plan": plan}
    steps: list[dict[str, Any]] = [
        _workflow({**common, "goals": _goals(bugs), "base": "main"}),
        {"text": "bug stage running"},
    ]
    if features:
        setup = " && ".join(
            [
                f"git -C {repo} fetch -q origin main",
                f"git -C {repo} push -q origin origin/main:refs/heads/{FEATURE}",
                f"gh pr create --draft --title 'grind: meta {META}' --head {FEATURE} "
                f"--base main --body 'Closes #{META}'",
            ]
        )
        feature = {
            "branch": FEATURE,
            "worktree": str(h.repo / ".clud" / "grind" / "worktrees" / "feature"),
            # fake_gh numbers PRs from 101: one per bug goal, then this draft.
            "pr": f"https://github.com/o/r/pull/{101 + len(bugs)}",
        }
        steps += [
            _bash(setup),
            _after(
                OK,
                _workflow(
                    {
                        **common,
                        "goals": _goals(features),
                        "base": FEATURE,
                        "plan_url": PLAN_URL,
                        "feature": feature,
                        "feature_merge": "later",
                        "problem_reporting": "issue",
                        "stuck_bugs": stuck,
                    }
                ),
            ),
            {"text": "feature stage running"},
        ]
    steps.append({"text": "GRIND_DONE"})
    return {"name": "main", "steps": steps}


def _run(
    h: Harness,
    bugs: list[str],
    features: list[str],
    *,
    depends: dict[str, list[str]] | None = None,
    stuck: list[str] | None = None,
) -> tuple[RunResult, dict[str, Any]]:
    stuck = stuck or []
    meta_before = _seed(h, sorted([*bugs, *features], key=int))
    plan = _plan(bugs, features, depends or {})
    roles = [_prework()]
    for g in bugs:
        roles += _goal(h, g, feature=False, stuck=g in stuck)
    for g in features:
        roles += _goal(h, g, feature=True)
    script = {"default_text": "OK", "roles": [*roles, _main(h, plan, bugs, features, stuck)]}
    result = h.run("start grind", script, timeout=900)
    assert result.returncode == 0, result.stdout[-3000:]
    return result, meta_before


# ---- helpers -------------------------------------------------------------------


def _text(value: Any) -> str:
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
    seen: dict[str, int] = {}
    for r in result.requests:
        seen[r["role"]] = min(seen.get(r["role"], r["n"]), r["n"])
    return seen


def _last(result: RunResult) -> dict[str, int]:
    seen: dict[str, int] = {}
    for r in result.requests:
        seen[r["role"]] = max(seen.get(r["role"], r["n"]), r["n"])
    return seen


def _no_notes(result: RunResult) -> None:
    notes = [(r["role"], r["note"]) for r in result.requests if r.get("note")]
    assert not notes, notes


def _told(result: RunResult) -> str:
    """Everything the main session saw: the workflow results arrive there as
    task notifications carrying the return value (never the log lines)."""
    seen = [r["messages"] for r in result.requests if r["role"] == "main"]
    return json.dumps(seen, ensure_ascii=False)


def _main_turn(result: RunResult, turn: int) -> int:
    """The request number of the router's `turn`-th step (0-based), skipping
    background requests (titles, summaries) that offer no tools."""
    return min(
        r["n"]
        for r in result.requests
        if r["role"] == "main" and r.get("turn") == turn and "Bash" in (r.get("tools") or [])
    )


def _prs(h: Harness) -> dict[str, dict[str, Any]]:
    return {p["head"]: p for p in h.read_gh_state().get("prs", [])}


def _origin_heads(h: Harness) -> list[str]:
    out = h.git("for-each-ref", "--format=%(refname:short)", "refs/heads", cwd=h.origin)
    return out.split()


def _plan_comments(h: Harness) -> list[list[str]]:
    calls = h.read_gh_state().get("calls", [])
    return [c for c in calls if c[:2] == ["issue", "comment"] and any(MARKER in w for w in c)]


def _meta_unchanged(h: Harness, before: dict[str, Any]) -> None:
    """H7: title, body and sub-issue list are as the user wrote them."""
    after = h.read_gh_state()["issues"][META]
    assert (after["title"], after["body"]) == (before["title"], before["body"]), after
    numbers = [s["number"] for s in after["sub_issues"]]
    assert numbers == [s["number"] for s in before["sub_issues"]], after
    assert after["state"] == "open", after


# ---- tests ---------------------------------------------------------------------


def test_t1_all_bugs_land_on_main_and_no_feature_branch_exists(harness: Harness) -> None:
    bugs = ["101", "102", "103"]
    result, before = _run(harness, bugs, [])
    _no_notes(result)
    prs = _prs(harness)
    issues = harness.read_gh_state()["issues"]
    for g in bugs:
        pr = prs[_branch(g)]
        assert pr["base"] == "main", pr
        assert f"Closes #{g}" in pr["body"], pr
        assert pr["state"] == "MERGED", pr
        assert not pr["draft"], pr
        assert issues[g]["state"] == "closed", (g, issues[g])
        assert "Base: origin/main" in _prompt(result, f"integrator:{g}")
    assert len(prs) == len(bugs), prs
    assert not [b for b in _origin_heads(harness) if b.startswith(f"grind/meta-{META}")]
    assert len(_plan_comments(harness)) == 1
    # The router gets the plan comment back to hand a feature-stage call.
    assert PLAN_URL in _told(result), _told(result)[-3000:]
    _meta_unchanged(harness, before)


def test_t2_all_feature_skips_the_bug_stage(harness: Harness) -> None:
    features = ["101", "102"]
    main_before = harness.git("rev-parse", "main", cwd=harness.origin)
    result, before = _run(harness, [], features)
    _no_notes(result)
    first = _first(result)
    # The bug-stage call only posted the plan: prework, and no goal agent,
    # before the router cut the feature branch (its step 2).
    cut = _main_turn(result, 2)
    assert first["prework"] < cut, first
    goal_roles = [r for r in first if r.split(":")[0] in ("planner", "worker", "lander")]
    assert all(first[r] > cut for r in goal_roles), (cut, first)
    assert len(_plan_comments(harness)) == 1, "the feature-stage call reposted the plan"
    assert harness.git("rev-parse", FEATURE, cwd=harness.origin) == main_before
    prs = _prs(harness)
    for g in features:
        pr = prs[_branch(g)]
        assert pr["base"] == FEATURE, pr
        assert pr["state"] == "MERGED", pr
        prompt = _prompt(result, f"integrator:{g}")
        assert f"Base: origin/{FEATURE}" in prompt, prompt[-3000:]
        assert f"Plan comment: {PLAN_URL}" in _prompt(result, f"planner:{g}")
    _meta_unchanged(harness, before)


def test_t3_t6_mixed_bugs_land_before_the_feature_branch_is_cut(harness: Harness) -> None:
    bugs, features = ["101", "103"], ["102", "104"]
    result, before = _run(harness, bugs, features, depends={"104": ["103"]})
    _no_notes(result)
    first, last = _first(result), _last(result)
    cut = _main_turn(result, 2)
    # T3: every bug lander finished before the router created the branch ref,
    # and no feature goal started before it.
    for b in bugs:
        assert last[f"chaperone:{b}"] < cut, (b, cut, last)
    for f in features:
        assert first[f"planner:{f}"] > cut, (f, cut, first)
    for b in bugs:
        tip = harness.git("rev-parse", _branch(b), cwd=harness.origin)
        harness.git("merge-base", "--is-ancestor", tip, FEATURE, cwd=harness.origin)
    # T6: bug PRs close their issue on main; feature goal PRs only reference it.
    prs = _prs(harness)
    issues = harness.read_gh_state()["issues"]
    for b in bugs:
        pr = prs[_branch(b)]
        assert (pr["base"], pr["state"]) == ("main", "MERGED"), pr
        assert f"Closes #{b}" in pr["body"], pr
        assert issues[b]["state"] == "closed", issues[b]
        prompt = _prompt(result, f"integrator:{b}")
        assert "Base: origin/main" in prompt, prompt[-3000:]
        assert "Feature-stage goal" not in prompt, prompt[-3000:]
    for f in features:
        pr = prs[_branch(f)]
        assert (pr["base"], pr["state"]) == (FEATURE, "MERGED"), pr
        assert f"Refs #{f}" in pr["body"], pr
        assert "Closes" not in pr["body"], pr
        assert issues[f]["state"] == "open", issues[f]
        prompt = _prompt(result, f"integrator:{f}")
        assert f"Base: origin/{FEATURE}" in prompt, prompt[-3000:]
        assert f"`Refs #{f}`, not Closes" in prompt, prompt[-3000:]
    # One plan comment for both calls.
    assert len(_plan_comments(harness)) == 1
    assert first["prework"] < min(first[f"planner:{b}"] for b in bugs), first
    _meta_unchanged(harness, before)


def test_t5_stuck_bug_blocks_only_its_dependents_across_calls(harness: Harness) -> None:
    bugs, features = ["101", "103"], ["102", "104"]
    result, before = _run(harness, bugs, features, depends={"104": ["103"]}, stuck=["103"])
    prs = _prs(harness)
    assert prs[_branch("101")]["state"] == "MERGED"
    assert prs[_branch("103")]["state"] == "OPEN", prs[_branch("103")]
    assert prs[_branch("102")]["state"] == "MERGED", prs
    assert _branch("104") not in prs, prs
    ran = _first(result)
    assert "planner:104" not in ran, ran
    assert "integrator:102" in ran, ran
    # The stuck bug's fix never reached main, so the feature branch lacks it.
    tip = harness.git("rev-parse", _branch("103"), cwd=harness.origin)
    ancestors = harness.git("rev-list", FEATURE, cwd=harness.origin).split()
    assert tip not in ancestors
    _no_notes(result)
    assert "blocked: bug #103" in _told(result), _told(result)[-3000:]
    _meta_unchanged(harness, before)

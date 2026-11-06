"""`grind-run` feature-branch mode on the real Claude Code (#1410, F1-F12 of #1392).

The router's feature setup gives a run a feature branch
(`grind/meta-100-1f3a`), its worktree at `<repo>/.clud/grind/worktrees/feature`
and a draft feature PR (#101, base `main`, `Closes #100`). Goal PRs target the
feature branch with `Refs #N`; once a goal PR merged, its lander posts the
`grind:v1` marker and adds the goal's `Closes` line to the feature PR (#1393
§2, S5). It never creates or applies a label in the target repo (#1958).
Once the goals settle, `feature_merge` decides: `auto` readies the feature PR
and merges it with `--merge` (never `--admin`), `decide_later` has the router
mark it ready and leave it open, `comment_only` keeps the draft and the router
posts one result comment on the meta issue. `main` moving is merged into the
feature branch, never rebased. While the run facts records the feature, the hook
also caps the main-session router: no issue close (S3), no feature PR merge,
no `grind/*` branch deletion.

The hook caps read the feature from the session's run facts (`feature: {branch,
worktree, pr}` and `feature_merge`). A denied call still shows in the hook
log (the recorder runs first) but never reaches `fake_gh`, so a deny is
asserted as "attempted by <agent_type>" plus "absent from gh calls/refs".

Numbering: the feature PR is #101 (the first PR in the world, either seeded
or created by the scripted router), so goal 102's PR is #102 and goal 103's
#103 in sequential mode. `fake_gh` resolves `grind/102` as PR #102.

Script note: a step's `expect` checks the tool results of the *previous*
step, so an expectation about a command sits on the step after it.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from tests.harness.harness import Harness, RunResult
from tests.harness.worlds import _issue, _world

MARK = {
    "planner": "You are the /grind planner",
    "worker": "You are the /grind editor",
    "reviewer": "You are the /grind reviewer",
    "integrator": "You are the /grind integrator",
    "lander": "You are the /grind lander",
}
META = "100"
RUN_ID = "1f3a"
FEATURE = f"grind/meta-{META}-{RUN_ID}"
PR = "101"
PR_URL = f"https://github.com/o/r/pull/{PR}"
GOALS = ["102", "103"]
OK = {"is_error": False}


def _structured(value: dict[str, Any]) -> dict[str, Any]:
    return {"tool_use": {"name": "StructuredOutput", "input": value}}


def _bash(command: str) -> dict[str, Any]:
    return {"tool_use": {"name": "Bash", "input": {"command": command, "description": "grind"}}}


def _after(expect: dict[str, Any], step: dict[str, Any]) -> dict[str, Any]:
    """`step`, first checking the previous step's tool results."""
    return {**step, "expect": expect}


def _denied(text: str) -> dict[str, Any]:
    return {"is_error": True, "content_contains": text}


def _role(
    name: str, role: str, prompt: str | list[str], steps: list[dict[str, Any]]
) -> dict[str, Any]:
    return {"name": name, "match": MARK[role], "match_prompt": prompt, "steps": steps}


def _wt(h: Harness) -> Path:
    return Path(h.repo) / ".clud" / "grind" / "worktrees" / "feature"


# ---- world ---------------------------------------------------------------------


def _router_setup(h: Harness) -> str:
    """What the `/grind` router does before the workflow: branch, worktree, draft PR."""
    repo = str(h.repo)
    return (
        f"git -C {repo} branch {FEATURE} main && git -C {repo} push -q -u origin {FEATURE} && "
        f"git -C {repo} worktree add -q {_wt(h)} {FEATURE} && "
        f"gh pr create --draft --title 'grind: meta {META}' --head {FEATURE} --base main "
        f"--body 'Closes #{META}'"
    )


def _seed(
    h: Harness,
    *,
    router: bool = False,
    pr_extra: dict[str, Any] | None = None,
    move_main: bool = False,
) -> None:
    """Meta #100 with feature children 102, 103; the feature set up unless `router`."""
    issues = {int(META): _issue("meta: auth rework", "Tracked as sub-issues.")}
    for n in (102, 103):
        issues[n] = _issue(f"child {n}", f"do {n}", parent=int(META))
    state = _world(issues)
    if not router:
        h.git("branch", FEATURE, "main")
        h.git("push", "-q", "-u", "origin", FEATURE)
        h.git("worktree", "add", "-q", str(_wt(h)), FEATURE)
        state["prs"] = [
            {
                "number": int(PR),
                "head": FEATURE,
                "state": "OPEN",
                "title": f"grind: meta {META}",
                "base": "main",
                "draft": True,
                "body": f"Closes #{META}",
                **(pr_extra or {}),
            }
        ]
    h.write_gh_state(state)
    if move_main:
        h.git("commit", "-q", "--allow-empty", "-m", "main moved")
        h.git("push", "-q", "origin", "main")


def _run_facts(h: Harness, *, mode: str, merge: str) -> None:
    run = h.run_facts_path()
    run.parent.mkdir(parents=True, exist_ok=True)
    facts = {
        "mode": mode,
        "meta": META,
        "feature": {"branch": FEATURE, "worktree": str(_wt(h)), "pr": PR_URL},
        "feature_merge": merge,
    }
    run.write_text(json.dumps(facts), encoding="utf-8")


# ---- scripted roles ------------------------------------------------------------


def _closes(goal: str) -> str:
    done = GOALS[: GOALS.index(goal) + 1]
    return " ".join(f"Closes #{n}" for n in [META, *done])


def _marker(goal: str | None) -> str:
    goal_pr = f" goal-pr=#{goal}" if goal else ""
    return f"<!-- grind:v1 feature-pr=#{PR} branch={FEATURE}{goal_pr} run={RUN_ID} -->"


def _record(goal: str) -> str:
    """What the lander runs once a goal PR merged into the feature branch (#1393 §2)."""
    commands = [
        f"gh issue comment {goal} --body 'Landed on {FEATURE} via #{goal}. {_marker(goal)}'",
    ]
    if goal == GOALS[0]:
        commands.append(f"gh issue comment {META} --body '{_marker(None)}'")
    commands.append(f"gh pr edit {PR} --body '{_closes(goal)}'")
    return " && ".join(commands)


def _goal_roles(
    h: Harness,
    goal: str,
    *,
    mode: str,
    merge_main: bool = False,
    lander: list[dict[str, Any]] | None = None,
) -> list[dict[str, Any]]:
    """One feature goal's planner, worker, reviewer, integrator and lander."""
    branch = f"grind/{goal}"
    repo = str(h.repo)
    at = f"Goal {goal}:"
    parallel = mode == "parallel"
    checkout = (
        str(h.home / ".clud" / "worktrees" / f"{h.repo.name}-wt-{goal}")
        if parallel
        else str(_wt(h))
    )
    plan = {
        "checkout": checkout,
        "branch": branch,
        "depends_on": [],
        "verify": "true",
        "tasks": [{"id": "t1", "files": [f"{goal}.txt"], "instructions": f"write {goal}.txt"}],
    }
    planner = [_structured(plan)]
    if parallel:
        add = (
            f"git -C {repo} fetch -q origin && "
            f"git -C {repo} worktree add --no-track -q {checkout} -b {branch} origin/{FEATURE}"
        )
        planner = [_bash(add), _after(OK, _structured(plan))]
    write = {"file_path": f"{checkout}/{goal}.txt", "content": f"{goal}\n"}
    git = f"git -C {checkout}"
    # One GitHub/remote mutation per command (#1979): only `git push && gh pr
    # create` may share one, so merging main in is its own step.
    integrate: list[dict[str, Any]] = []
    if merge_main:
        integrate.append(
            _bash(
                f"{git} fetch -q origin && "
                f"{git} merge -q --no-ff origin/main -m 'merge main into {FEATURE}' && "
                f"{git} push -q origin HEAD:{FEATURE}"
            )
        )
    steps = [f"{git} fetch -q origin"]
    if not parallel:
        steps.append(f"{git} switch -q -c {branch} origin/{FEATURE}")
    steps += [
        f"{git} add {goal}.txt",
        f"{git} commit -q -m {goal}",
        f"{git} push -q -u origin {branch}",
        f"gh pr create --title {goal} --head {branch} --base {FEATURE} --body 'Refs #{goal}'",
    ]
    url = f"https://github.com/o/r/pull/{branch}"
    land = lander or [
        _bash(f"gh pr merge {branch} --merge"),
        _after(OK, _bash(_record(goal))),
        _after(OK, _structured({"status": "merged", "summary": "green"})),
    ]
    return [
        _role(f"planner:{goal}", "planner", at, planner),
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
                s if i == 0 else _after(OK, s)
                for i, s in enumerate(
                    [
                        *integrate,
                        _bash(" && ".join(steps)),
                        _structured({"pushed": True, "pr_url": url, "summary": "p"}),
                    ]
                )
            ],
        ),
        _role(f"lander:{goal}", "lander", [at, "Fix rounds used: 0 of"], land),
    ]


def _feature_lander(steps: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    steps = steps or [
        _bash(f"gh pr ready {PR}"),
        _after(OK, _bash(f"gh pr merge {PR} --merge")),
        _after(OK, _structured({"status": "merged", "summary": "feature merged"})),
    ]
    return _role("lander:feature", "lander", "for the FEATURE PR", steps)


def _script(
    h: Harness,
    roles: list[dict[str, Any]],
    *,
    mode: str,
    merge: str,
    pre: list[dict[str, Any]],
    post: list[dict[str, Any]],
) -> dict[str, Any]:
    args: dict[str, Any] = {
        "repo": str(h.repo),
        "github_repo": "o/r",
        "main": "main",
        "mode": mode,
        "meta": META,
        "goals": [{"id": g, "title": f"child {g}", "brief": f"do {g}"} for g in GOALS],
        "feature": {"branch": FEATURE, "worktree": str(_wt(h)), "pr": PR_URL},
        "feature_merge": merge,
    }
    workflow = {"tool_use": {"name": "Workflow", "input": {"name": "grind-run", "args": args}}}
    steps = [*pre, _after(OK, workflow) if pre else workflow, *post, {"text": "GRIND_DONE"}]
    return {"default_text": "OK", "roles": [*roles, {"name": "main", "steps": steps}]}


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


def _no_notes(result: RunResult) -> None:
    notes = [(r["role"], r["note"]) for r in result.requests if r.get("note")]
    assert not notes, notes


def _told(result: RunResult) -> str:
    seen = [r["messages"] for r in result.requests if r["role"] == "main"]
    return json.dumps(seen, ensure_ascii=False) + result.stdout


def _pr(h: Harness, number: str) -> dict[str, Any]:
    return next(p for p in h.read_gh_state()["prs"] if str(p["number"]) == number)


def _goal_pr(h: Harness, goal: str) -> dict[str, Any]:
    return next(p for p in h.read_gh_state()["prs"] if p.get("head") == f"grind/{goal}")


def _calls(h: Harness, *head: str) -> list[list[str]]:
    return [c for c in h.read_gh_state().get("calls", []) if c[: len(head)] == list(head)]


def _origin(h: Harness, rev: str) -> str:
    return h.git("rev-parse", rev, cwd=h.origin)


def _origin_has(h: Harness, branch: str) -> bool:
    return bool(h.git("for-each-ref", f"refs/heads/{branch}", cwd=h.origin))


def _attempted(result: RunResult, agent_type: str, needle: str) -> bool:
    """Whether `agent_type` tried a Bash command containing `needle` (hook log)."""
    return any(
        r.get("agent_type") == agent_type
        and r.get("tool_name") == "Bash"
        and needle in str((r.get("tool_input") or {}).get("command", ""))
        for r in result.hooks
    )


def _goals(h: Harness, mode: str, **kw: Any) -> list[dict[str, Any]]:
    landers = kw.pop("landers", {})
    roles: list[dict[str, Any]] = []
    for g in GOALS:
        roles += _goal_roles(h, g, mode=mode, lander=landers.get(g), **kw)
    return roles


def _run(
    h: Harness,
    roles: list[dict[str, Any]],
    *,
    mode: str = "sequential",
    merge: str = "decide_later",
    router: bool = False,
    pr_extra: dict[str, Any] | None = None,
    move_main: bool = False,
    post: list[dict[str, Any]] | None = None,
) -> RunResult:
    _seed(h, router=router, pr_extra=pr_extra, move_main=move_main)
    _run_facts(h, mode=mode, merge=merge)
    pre = [_bash(_router_setup(h))] if router else []
    script = _script(h, roles, mode=mode, merge=merge, pre=pre, post=post or [])
    result = h.run("start grind", script, timeout=600)
    assert result.returncode == 0, result.stdout[-3000:]
    return result


# ---- tests ---------------------------------------------------------------------


def test_f1_setup_opens_the_draft_feature_pr_before_any_goal_merges(harness: Harness) -> None:
    main_sha = harness.git("rev-parse", "main")
    result = _run(harness, _goals(harness, "sequential"), router=True)
    _no_notes(result)
    pr = _pr(harness, PR)
    assert pr["head"] == FEATURE, pr
    assert pr["base"] == "main", pr
    assert pr["draft"] is True, pr
    assert pr["state"] == "OPEN", pr
    assert f"Closes #{META}" in pr["body"]
    assert _origin(harness, FEATURE) == main_sha
    assert str(_wt(harness)) in harness.git("worktree", "list")
    calls = harness.read_gh_state()["calls"]
    created = next(i for i, c in enumerate(calls) if c[:2] == ["pr", "create"] and "--draft" in c)
    merged = next(i for i, c in enumerate(calls) if c[:2] == ["pr", "merge"])
    assert created < merged, calls
    assert _origin(harness, "main") == main_sha


def test_f2_goal_prs_target_the_feature_branch_and_main_is_unchanged(harness: Harness) -> None:
    main_sha = harness.git("rev-parse", "main")
    result = _run(harness, _goals(harness, "sequential"))
    _no_notes(result)
    for g in GOALS:
        pr = _goal_pr(harness, g)
        assert pr["base"] == FEATURE, pr
        assert f"Refs #{g}" in pr["body"], pr
        assert "Closes" not in pr["body"], pr
        text = _prompt(result, f"integrator:{g}")
        assert f"Base: origin/{FEATURE}" in text, text[-3000:]
        assert "Base: origin/main" not in text
        assert f"Feature-stage goal: merge this PR into the feature branch {FEATURE}" in _prompt(
            result, f"lander:{g}"
        )
    assert _origin(harness, "main") == main_sha
    # Goal merges land on the feature branch, so they close nothing yet.
    issues = harness.read_gh_state()["issues"]
    assert all(issues[n]["state"] == "open" for n in (META, *GOALS)), issues


def test_f3_feature_pr_body_collects_closes_lines(harness: Harness) -> None:
    result = _run(harness, _goals(harness, "sequential"))
    _no_notes(result)
    body = _pr(harness, PR)["body"]
    for n in (META, *GOALS):
        assert f"Closes #{n}" in body, body
    # The lander adds a goal's Closes line once the goal landed; the
    # integrator never touches the feature PR (a goal that never lands must
    # not be closed by the feature merge).
    for g in GOALS:
        land = _prompt(result, f"lander:{g}")
        assert f"Closes #{g}" in land, land[-3000:]
        assert _marker("<n>") in land, land[-3000:]
        assert "Do not edit the feature PR" in _prompt(result, f"integrator:{g}")
    assert _attempted(result, "grind-lander", f"gh pr edit {PR}")
    assert not _attempted(result, "grind-integrator", "gh pr edit")
    assert _calls(harness, "pr", "edit", PR), harness.read_gh_state()["calls"]


def test_s5_landed_goals_get_the_marker_and_no_label(harness: Harness) -> None:
    result = _run(harness, _goals(harness, "sequential"))
    _no_notes(result)
    issues = harness.read_gh_state()["issues"]
    # The marker is grind's only trace on the issue: no label (#1958).
    for g in (*GOALS, META):
        assert not issues[g]["labels"], issues[g]
    for g in GOALS:
        assert any(_marker(g) in c["body"] for c in issues[g]["comments"]), issues[g]
    assert not _calls(harness, "label"), harness.read_gh_state()["calls"]
    metas = [c for c in issues[META]["comments"] if f"feature-pr=#{PR}" in c["body"]]
    assert len(metas) == 1, issues[META]["comments"]
    # Landing on the feature branch closes nothing.
    assert all(issues[n]["state"] == "open" for n in (META, *GOALS)), issues


def test_s3_router_cannot_close_an_issue_in_feature_mode(harness: Harness) -> None:
    _seed(harness)
    _run_facts(harness, mode="sequential", merge="auto")
    close = f"gh issue close {GOALS[0]}"
    steps = [
        _bash(close),
        _after(_denied("never closes issues"), {"text": "GRIND_DONE"}),
    ]
    script = {"default_text": "OK", "roles": [{"name": "main", "steps": steps}]}
    result = harness.run("start grind", script, timeout=300)
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)
    assert any(
        r.get("tool_name") == "Bash"
        and not r.get("agent_type")
        and close in str((r.get("tool_input") or {}).get("command", ""))
        for r in result.hooks
    )
    assert not _calls(harness, "issue", "close")
    assert harness.read_gh_state()["issues"][GOALS[0]]["state"] == "open"


def test_f4_auto_readies_then_merge_commits_and_closes_issues(harness: Harness) -> None:
    roles = [*_goals(harness, "sequential"), _feature_lander()]
    result = _run(harness, roles, merge="auto")
    _no_notes(result)
    ready = _calls(harness, "pr", "ready", PR)
    merge = _calls(harness, "pr", "merge", PR)
    assert ready, harness.read_gh_state()["calls"]
    assert merge, harness.read_gh_state()["calls"]
    calls = harness.read_gh_state()["calls"]
    assert calls.index(ready[0]) < calls.index(merge[0])
    assert "--merge" in merge[0], merge
    assert "--admin" not in merge[0], merge
    pr = _pr(harness, PR)
    assert pr["state"] == "MERGED", pr
    assert pr["merge_method"] == "merge", pr
    assert pr["admin"] is False, pr
    assert pr["base"] == "main", pr
    issues = harness.read_gh_state()["issues"]
    for n in (META, *GOALS):
        assert issues[n]["state"] == "closed", (n, issues[n])
        assert issues[n]["closed_by"] == {"kind": "pr", "pr": int(PR)}, (n, issues[n])
    assert _first(result)["lander:feature"] > max(_first(result)[f"lander:{g}"] for g in GOALS)


def test_f5_required_review_leaves_the_pr_open_waiting_for_review(harness: Harness) -> None:
    lander = _feature_lander(
        [
            _bash(f"gh pr ready {PR}"),
            _after(OK, _bash(f"gh pr merge {PR} --merge")),
            _after(
                _denied("review required"),
                _structured({"status": "gave_up", "summary": "waiting for review"}),
            ),
        ]
    )
    roles = [*_goals(harness, "sequential"), lander]
    result = _run(
        harness, roles, merge="auto", pr_extra={"reviews_required": True, "approved": False}
    )
    _no_notes(result)
    pr = _pr(harness, PR)
    assert pr["state"] == "OPEN", pr
    merges = _calls(harness, "pr", "merge", PR)
    assert merges, merges
    assert all("--admin" not in c for c in merges), merges
    assert not _attempted(result, "grind-lander", "--admin")
    assert "waiting for review" in _told(result), _told(result)[-3000:]
    issues = harness.read_gh_state()["issues"]
    assert issues[META]["state"] == "open"


def test_f6_hook_denies_admin_merge_of_the_feature_pr(harness: Harness) -> None:
    admin = f"gh pr merge {PR} --admin --merge"
    lander = _feature_lander(
        [
            _bash(f"gh pr ready {PR}"),
            _after(OK, _bash(admin)),
            _after(_denied("--admin"), _bash(f"gh pr merge {PR} --merge")),
            _after(OK, _structured({"status": "merged", "summary": "feature merged"})),
        ]
    )
    result = _run(harness, [*_goals(harness, "sequential"), lander], merge="auto")
    _no_notes(result)
    assert _attempted(result, "grind-lander", admin)
    merges = _calls(harness, "pr", "merge", PR)
    assert merges, merges
    assert all("--admin" not in c for c in merges), merges
    assert _pr(harness, PR)["admin"] is False


def _main_attempted(result: RunResult, needle: str) -> bool:
    """Whether the main-session router tried a Bash command containing `needle`."""
    return any(
        not r.get("agent_type")
        and r.get("tool_name") == "Bash"
        and needle in str((r.get("tool_input") or {}).get("command", ""))
        for r in result.hooks
    )


def test_f7_decide_later_readies_the_pr_and_denies_merging_it(harness: Harness) -> None:
    denied = f"gh pr merge {PR} --merge"
    lander = [
        _bash(denied),
        _after(_denied("decide later"), _bash("gh pr merge grind/102 --merge")),
        _after(OK, _bash(_record("102"))),
        _after(OK, _structured({"status": "merged", "summary": "green"})),
    ]
    roles = _goals(harness, "sequential", landers={"102": lander})
    # Finish under `later`: every goal landed, so the router marks the PR
    # ready; merging it is the user's call, and the hook refuses the router.
    post = [
        _after(OK, _bash(f"gh pr ready {PR}")),
        _after(OK, _bash(denied)),
    ]
    result = _run(harness, roles, merge="decide_later", post=post)
    _no_notes(result)
    assert _attempted(result, "grind-lander", denied)
    assert _main_attempted(result, denied)
    assert not _calls(harness, "pr", "merge", PR)
    assert _calls(harness, "pr", "ready", PR)
    assert not _attempted(result, "grind-lander", f"gh pr ready {PR}")
    pr = _pr(harness, PR)
    assert pr["state"] == "OPEN", pr
    assert pr["draft"] is False, pr
    assert "lander:feature" not in _first(result)
    assert harness.read_gh_state()["issues"][META]["state"] == "open"


def test_f8_comment_only_keeps_the_draft_and_posts_one_result_comment(harness: Harness) -> None:
    post = [
        _after(
            OK,
            _bash(f"gh issue comment {META} --body 'grind result: feature PR {PR_URL} ready'"),
        ),
    ]
    result = _run(harness, _goals(harness, "sequential"), merge="comment_only", post=post)
    _no_notes(result)
    pr = _pr(harness, PR)
    assert pr["state"] == "OPEN", pr
    assert pr["draft"] is True, pr
    assert not _calls(harness, "pr", "ready", PR)
    assert not _calls(harness, "pr", "merge", PR)
    # The meta issue also carries the lander's grind:v1 marker comment.
    comments = harness.read_gh_state()["issues"][META]["comments"]
    results = [c for c in comments if "grind result" in c["body"]]
    assert len(results) == 1, comments
    assert "lander:feature" not in _first(result)


def test_f9_moving_main_is_merged_in_never_rebased(harness: Harness) -> None:
    feature_before = harness.git("rev-parse", "main")
    roles = _goal_roles(harness, "102", mode="sequential", merge_main=True)
    roles += _goal_roles(harness, "103", mode="sequential")
    result = _run(harness, roles, move_main=True)
    _no_notes(result)
    main_after = _origin(harness, "main")
    assert main_after != feature_before
    tip = _origin(harness, FEATURE)
    parents = harness.git("rev-list", "--parents", "-n", "1", tip, cwd=harness.origin).split()
    assert parents[1:] == [feature_before, main_after], parents
    # The goal branch sits on the merge commit; history below it is untouched.
    assert _origin(harness, "grind/102^") == tip
    assert _origin(harness, f"{FEATURE}^1") == feature_before
    text = _prompt(result, "integrator:102")
    assert "merge main into the feature branch" in text, text[-3000:]
    assert "Never rebase the feature branch" in text


def test_f10_parallel_goal_worktrees_are_based_on_the_feature_branch(harness: Harness) -> None:
    feature = harness.git("rev-parse", "main")
    # Plain merges only: fake_gh's state file has no lock, so two landers'
    # bookkeeping calls at once could lose an update (F3/S5 cover it).
    plain = {
        g: [
            _bash(f"gh pr merge grind/{g} --merge"),
            _after(OK, _structured({"status": "merged", "summary": "green"})),
        ]
        for g in GOALS
    }
    result = _run(harness, _goals(harness, "parallel", landers=plain), mode="parallel")
    _no_notes(result)
    for g in GOALS:
        text = _prompt(result, f"planner:{g}")
        assert f"from origin/{FEATURE}, not origin/main" in text, text[-3000:]
        assert _attempted(result, "grind-planner", f"-b grind/{g} origin/{FEATURE}")
        assert _origin(harness, f"grind/{g}^") == feature
        assert _goal_pr(harness, g)["base"] == FEATURE


def test_f11_sequential_goals_run_in_the_feature_worktree(harness: Harness) -> None:
    result = _run(harness, _goals(harness, "sequential"))
    _no_notes(result)
    wt = str(_wt(harness))
    for g in GOALS:
        assert f"work in the feature worktree {wt}" in _prompt(result, f"planner:{g}")
        assert f"Checkout: {wt}" in _prompt(result, f"worker:{g}")
        assert not (Path(harness.repo) / f"{g}.txt").exists()
        assert _origin_has(harness, f"grind/{g}")
        # Each goal was committed in the feature worktree on its own branch.
        # fake_gh's merge moves no git refs, so goal 103 branches from an
        # origin/<feature> without 102.txt and the worktree ends on grind/103.
        assert harness.git("show", f"grind/{g}:{g}.txt", cwd=harness.origin) == g
    assert harness.git("branch", "--show-current", cwd=Path(wt)) == f"grind/{GOALS[-1]}"
    assert (Path(wt) / f"{GOALS[-1]}.txt").is_file()
    assert harness.git("branch", "--show-current") == "main"


def test_f12_no_grind_branch_deletion_while_the_feature_pr_is_open(harness: Harness) -> None:
    delete = "git push origin --delete grind/102"
    lander = [
        _bash("gh pr merge grind/102 --merge"),
        _after(OK, _bash(delete)),
        _after(
            _denied("may not delete grind/* branches"),
            _structured({"status": "merged", "summary": "green"}),
        ),
    ]
    roles = _goals(harness, "sequential", landers={"102": lander})
    # Finish: the router may not delete grind/* branches either while the
    # feature PR is open.
    router_delete = f"git -C {harness.repo} push origin --delete grind/103 {FEATURE}"
    post = [_after(OK, _bash(router_delete))]
    result = _run(harness, roles, post=post)
    _no_notes(result)
    assert _attempted(result, "grind-lander", delete)
    assert _main_attempted(result, router_delete)
    for branch in ("grind/102", "grind/103", FEATURE):
        assert _origin_has(harness, branch), branch
    assert _pr(harness, PR)["state"] == "OPEN"

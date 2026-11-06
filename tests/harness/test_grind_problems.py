"""`/grind` problem reporting on the real Claude Code (#1411, cases X1-X8 of #1392).

Every grind role may return `problems: [{kind, summary, evidence,
related_issue}]`. The `grind-run` workflow collects them once per run
(deduplicated across roles and fix rounds) and returns them; only the
router (the main session) files them, per the plan's `problem_reporting`:

- `issue`: one `grind:followup` issue per problem, with `Refs #<meta>` and
  a `<!-- grind:followup ... -->` marker, never a sub-issue of the meta;
- `comment`: one comment per problem on its `related_issue`, else on the meta.

The main session is scripted, so the router's filing commands are fixed in
the script. What these tests guard is the contract around them: the
workflow hands the router each problem exactly once, the hook lets the router
(and only the router) file, the fake GitHub ends up in the right shape, and
a follow-up never blocks the meta issue.

The Workflow tool runs in the background: its call returns at once and the
workflow's result reaches the main session later as a `<task-notification>`
user turn. The router's filing steps answer that turn (a second main role,
picked only once the notification is in the conversation).

Script note: a step's `expect` checks the tool results of the *previous*
step, so an expectation about a command sits on the step after it.
"""

from __future__ import annotations

import json
import re
import shlex
from typing import Any

from tests import process
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
GOAL = "101"
MARKER = "grind:followup"
OK = {"is_error": False}
DENIED = {"is_error": True, "content_contains": "/grind role caps"}
NOTIFIED = "</task-notification>"
FLAKY = {
    "kind": "flaky-test",
    "summary": "test_cache_eviction flakes under load",
    "evidence": "failed 1 of 5 local runs with a timeout",
}
DOCS = {
    "kind": "doc-gap",
    "summary": "README omits the --verbose flag",
    "evidence": "README.md has no mention of --verbose",
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


# ---- world ---------------------------------------------------------------------


def _seed(h: Harness, extra: dict[int, dict[str, Any]] | None = None) -> None:
    """Meta #100 with #101 as its only sub-issue, plus any `extra` issues."""
    issues = {int(META): _issue("meta: backlog", "Tracked as sub-issues.")}
    issues[int(GOAL)] = _issue(f"child {GOAL}", f"do {GOAL}", parent=int(META))
    issues.update(extra or {})
    h.write_gh_state(_world(issues))


def _run_facts(h: Harness, reporting: str) -> None:
    run = h.run_facts_path()
    run.parent.mkdir(parents=True, exist_ok=True)
    facts = {"mode": "sequential", "meta": META, "problem_reporting": reporting}
    run.write_text(json.dumps(facts), encoding="utf-8")


# ---- scripted roles ------------------------------------------------------------


def _goal_roles(
    h: Harness,
    *,
    worker_problems: list[dict[str, Any]] | None = None,
    integrator_problems: list[dict[str, Any]] | None = None,
    fix_round_problems: list[dict[str, Any]] | None = None,
    worker_tries_create: bool = False,
) -> list[dict[str, Any]]:
    """Goal #101's roles. With `fix_round_problems`, one fix round runs.

    With `worker_tries_create`, the worker first tries to file its problem
    itself (`gh issue create`), which the hook denies.
    """
    goal = GOAL
    branch = f"grind/{goal}"
    repo = str(h.repo)
    at = f"Goal {goal}:"
    plan = {
        "checkout": repo,
        "branch": branch,
        "depends_on": [],
        "verify": "true",
        "tasks": [{"id": "t1", "files": [f"{goal}.txt"], "instructions": f"write {goal}.txt"}],
    }
    written = {"file_path": f"{repo}/{goal}.txt", "content": f"{goal}\n"}
    write = {"tool_use": {"name": "Write", "input": written}}
    push = (
        f"git -C {repo} switch -q main && git -C {repo} switch -q -c {branch} && "
        f"git -C {repo} add {goal}.txt && git -C {repo} commit -q -m {goal} && "
        f"git -C {repo} push -q -u origin {branch} && "
        f"gh pr create --title {goal} --head {branch} --body 'Closes #{goal}'"
    )
    url = f"https://github.com/o/r/pull/{branch}"
    work: dict[str, Any] = {"files_touched": [f"{goal}.txt"], "summary": "wrote"}
    if worker_problems:
        work["problems"] = worker_problems
    worker_steps = [write, _after(OK, _structured(work))]
    if worker_tries_create:
        create = _bash("gh issue create --title 'flaky-test: self-filed' --body x")
        worker_steps = [create, _after(DENIED, write), _after(OK, _structured(work))]
    integ: dict[str, Any] = {"pushed": True, "pr_url": url, "summary": "p"}
    if integrator_problems:
        integ["problems"] = integrator_problems
    merge = [
        _bash(f"gh pr merge {branch} --admin --squash"),
        _after(OK, _structured({"status": "merged", "summary": "green"})),
    ]
    roles: list[dict[str, Any]] = []
    if fix_round_problems is not None:
        # Most specific first: the fix-round integrator and both lander rounds.
        fixed = {"pushed": True, "pr_url": url, "summary": "fixed", "problems": fix_round_problems}
        needs_fix = {"status": "needs_fix", "summary": "red", "failure_log": "ci red"}
        roles += [
            _role("integrator:fix", "integrator", [at, "FIX ROUND 1 of"], [_structured(fixed)]),
            _role("lander:1", "lander", [at, "Fix rounds used: 1 of"], merge),
            _role("lander:0", "lander", [at, "Fix rounds used: 0 of"], [_structured(needs_fix)]),
        ]
    else:
        roles.append(_role("lander:0", "lander", [at, "Fix rounds used: 0 of"], merge))
    return [
        _role("planner", "planner", at, [_structured(plan)]),
        _role("worker", "worker", at, worker_steps),
        _role("reviewer", "reviewer", at, [_structured({"approved": True, "summary": "ok"})]),
        *roles,
        _role(
            "integrator",
            "integrator",
            [at, "Reviewer summary:"],
            [_bash(push), _after(OK, _structured(integ))],
        ),
    ]


def _script(
    h: Harness, roles: list[dict[str, Any]], router: list[dict[str, Any]]
) -> dict[str, Any]:
    """Main starts the workflow; once notified of its result, runs `router`.

    The second main role is chosen only after the `<task-notification>` is
    in the conversation; its steps are padded to the two assistant turns the
    first one took (the Workflow call and its text).
    """
    args = {
        "repo": str(h.repo),
        "github_repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "meta": META,
        "goals": [{"id": GOAL, "title": f"child {GOAL}", "brief": f"do {GOAL}"}],
    }
    start = [
        {"tool_use": {"name": "Workflow", "input": {"name": "grind-run", "args": args}}},
        {"text": "GRIND_STARTED"},
    ]
    pad = [{"text": "unused"}] * len(start)
    finish = {"name": "main", "match_prompt": NOTIFIED, "steps": [*pad, *router]}
    return {"default_text": "OK", "roles": [*roles, finish, {"name": "main", "steps": start}]}


# ---- router filing steps -------------------------------------------------------


def _followup_body(p: dict[str, Any]) -> str:
    return (
        f"{p['evidence']} Refs #{META} "
        f"<!-- grind:followup meta={META} stage=bugs feature-pr=none -->"
    )


def _file_issue(p: dict[str, Any]) -> str:
    search = shlex.quote(f'"{MARKER}" {p["summary"]} in:title,body')
    title = shlex.quote(f"{p['kind']}: {p['summary']}")
    return (
        f"gh issue list --state all --search {search} && "
        f"gh issue create --title {title} --body {shlex.quote(_followup_body(p))}"
    )


def _file_comment(target: str, p: dict[str, Any]) -> str:
    body = shlex.quote(f"grind problem ({p['kind']}): {p['summary']}. {p['evidence']}")
    return f"gh issue comment {target} --body {body}"


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


def _no_notes(result: RunResult) -> None:
    notes = [(r["role"], r["note"]) for r in result.requests if r.get("note")]
    assert not notes, notes


def _router_saw(result: RunResult) -> str:
    """The main session's last request: it holds the workflow's result."""
    mains = [r for r in result.requests if r["role"] == "main"]
    assert mains, [r["role"] for r in result.requests]
    return _text(mains[-1].get("messages"))


def _notified(result: RunResult, *problems: dict[str, Any]) -> str:
    """The router saw the workflow's result, and it names every problem."""
    seen = _router_saw(result)
    assert NOTIFIED in seen, seen[-3000:]
    for p in problems:
        assert p["summary"] in seen, (p["summary"], seen[-3000:])
    return seen


def _followups(h: Harness) -> dict[str, dict[str, Any]]:
    issues = h.read_gh_state()["issues"]
    return {n: i for n, i in issues.items() if f"<!-- {MARKER}" in i.get("body", "")}


def _bash_by(result: RunResult, agent_type: str) -> list[str]:
    return [
        str(h.get("tool_input", {}).get("command", ""))
        for h in result.hooks
        if h.get("agent_type") == agent_type
        and h.get("event") == "PreToolUse"
        and h.get("tool_name") == "Bash"
    ]


def _run(h: Harness, script: dict[str, Any]) -> RunResult:
    result = h.run("start grind", script, timeout=600)
    assert result.returncode == 0, result.stdout[-3000:]
    return result


def _issue_creates(h: Harness) -> list[list[str]]:
    return [c for c in h.read_gh_state()["calls"] if c[:2] == ["issue", "create"]]


# ---- tests ---------------------------------------------------------------------


def test_x1_issue_policy_files_one_followup_that_is_not_a_sub_issue(harness: Harness) -> None:
    _seed(harness)
    _run_facts(harness, "issue")
    router = [_bash(_file_issue(FLAKY)), _after(OK, {"text": "GRIND_DONE"})]
    roles = _goal_roles(harness, worker_problems=[FLAKY])
    result = _run(harness, _script(harness, roles, router))
    _no_notes(result)
    _notified(result, FLAKY)
    followups = _followups(harness)
    assert len(followups) == 1, followups
    ((number, issue),) = followups.items()
    assert f"Refs #{META}" in issue["body"], issue["body"]
    assert FLAKY["summary"] in issue["title"]
    state = harness.read_gh_state()["issues"]
    assert int(number) not in [s["number"] for s in state[META]["sub_issues"]]
    assert issue.get("parent") is None
    worker = _bash_by(result, "grind-editor")
    assert not [c for c in worker if "gh issue create" in c], worker
    # The router, not a subagent, made the only issue-create call.
    assert len(_issue_creates(harness)) == 1, _issue_creates(harness)


def test_x2_comment_policy_comments_on_the_related_child(harness: Harness) -> None:
    _seed(harness)
    _run_facts(harness, "comment")
    problem = {**FLAKY, "related_issue": GOAL}
    router = [_bash(_file_comment(GOAL, problem)), _after(OK, {"text": "GRIND_DONE"})]
    roles = _goal_roles(harness, worker_problems=[problem])
    result = _run(harness, _script(harness, roles, router))
    _no_notes(result)
    _notified(result, problem)
    issues = harness.read_gh_state()["issues"]
    child = [c for c in issues[GOAL].get("comments", []) if FLAKY["summary"] in c["body"]]
    assert len(child) == 1, issues[GOAL].get("comments")
    assert not [c for c in issues[META].get("comments", []) if FLAKY["summary"] in c["body"]]
    assert not _followups(harness)
    assert set(issues) == {META, GOAL}


def test_x3_comment_policy_without_related_issue_comments_on_meta(harness: Harness) -> None:
    _seed(harness)
    _run_facts(harness, "comment")
    router = [_bash(_file_comment(META, FLAKY)), _after(OK, {"text": "GRIND_DONE"})]
    roles = _goal_roles(harness, worker_problems=[FLAKY])
    result = _run(harness, _script(harness, roles, router))
    _no_notes(result)
    _notified(result, FLAKY)
    issues = harness.read_gh_state()["issues"]
    meta = [c for c in issues[META].get("comments", []) if FLAKY["summary"] in c["body"]]
    assert len(meta) == 1, issues[META].get("comments")
    assert not [c for c in issues[GOAL].get("comments", []) if FLAKY["summary"] in c["body"]]
    assert set(issues) == {META, GOAL}


def test_x4_duplicate_problems_across_roles_and_fix_rounds_are_filed_once(
    harness: Harness,
) -> None:
    _seed(harness)
    _run_facts(harness, "issue")
    router = [
        _bash(_file_issue(FLAKY)),
        _after(OK, _bash(_file_issue(DOCS))),
        _after(OK, {"text": "GRIND_DONE"}),
    ]
    roles = _goal_roles(
        harness,
        worker_problems=[FLAKY],
        integrator_problems=[FLAKY],
        fix_round_problems=[FLAKY, DOCS],
    )
    result = _run(harness, _script(harness, roles, router))
    _no_notes(result)
    ran = {r["role"] for r in result.requests}
    assert {"integrator:fix", "lander:1"} <= ran, ran
    # The workflow hands the router each problem once, however often it was
    # reported. The router's own commands quote the summary too, but never
    # as a `summary` field (plain or JSON-escaped).
    seen = _notified(result, FLAKY, DOCS)
    for p in (FLAKY, DOCS):
        field = r'summary\\?"?\s*:\s*\\?"?' + re.escape(p["summary"])
        entries = re.findall(field, seen)
        assert len(entries) == 1, (p["summary"], seen[-3000:])
    followups = _followups(harness)
    titles = sorted(i["title"] for i in followups.values())
    assert titles == sorted(f"{p['kind']}: {p['summary']}" for p in (DOCS, FLAKY)), titles


def test_x5_subagent_issue_create_is_denied(harness: Harness) -> None:
    # A worker that tries to file its own problem is refused by the hook; it
    # returns the problem instead and the router files it.
    _seed(harness)
    _run_facts(harness, "issue")
    router = [_bash(_file_issue(FLAKY)), _after(OK, {"text": "GRIND_DONE"})]
    roles = _goal_roles(harness, worker_problems=[FLAKY], worker_tries_create=True)
    result = _run(harness, _script(harness, roles, router))
    _no_notes(result)
    worker = _bash_by(result, "grind-editor")
    assert [c for c in worker if "gh issue create" in c], worker
    creates = _issue_creates(harness)
    assert len(creates) == 1, creates
    assert "self-filed" not in json.dumps(creates)
    assert len(_followups(harness)) == 1


def test_x6_open_followup_does_not_block_the_meta_close(harness: Harness) -> None:
    _seed(harness)
    _run_facts(harness, "issue")
    router = [_bash(_file_issue(FLAKY)), _after(OK, {"text": "GRIND_DONE"})]
    roles = _goal_roles(harness, worker_problems=[FLAKY])
    result = _run(harness, _script(harness, roles, router))
    _no_notes(result)
    issues = harness.read_gh_state()["issues"]
    followups = _followups(harness)
    assert len(followups) == 1, followups
    assert all(i["state"] == "open" for i in followups.values())
    # The meta issue's close depends on its sub-issues only; the open
    # follow-up is not among them, so it cannot hold the meta open.
    subs = issues[META]["sub_issues"]
    assert [s["number"] for s in subs] == [int(GOAL)], subs
    assert all(s["state"] == "closed" for s in subs), subs
    assert issues[GOAL]["closed_by"]["kind"] == "pr", issues[GOAL]
    assert not set(followups) & {str(s["number"]) for s in subs}


def test_x6_reconcile_closes_the_meta_despite_an_open_followup(harness: Harness) -> None:
    # The feature PR merged into main but left the meta open: `clud grind
    # reconcile` closes it, and the open follow-up neither stops that nor is
    # touched by it.
    feature_pr, branch, run_id = 150, f"grind/meta-{META}-1f3a", "1f3a"
    marker = f"<!-- grind:v1 feature-pr=#{feature_pr} branch={branch} run={run_id} -->"
    meta = _issue("meta: auth rework", "Tracked as sub-issues.")
    followup = _issue(
        "flaky-test: feature-stage follow-up",
        f"x Refs #{META} <!-- grind:followup meta={META} stage=feature "
        f"feature-pr={feature_pr} -->",
    )
    state = _world({int(META): meta, 201: followup})
    state["issues"][META]["comments"].append({"id": 2000, "body": marker})
    state["prs"] = [
        {
            "number": feature_pr,
            "head": branch,
            "state": "MERGED",
            "title": f"grind: meta {META}",
            "base": "main",
            "body": f"Closes #{META}",
        }
    ]
    harness.write_gh_state(state)
    ran = process.run(
        [str(harness.clud), "grind", "reconcile"],
        cwd=str(harness.repo),
        capture_output=True,
        text=True,
        env=harness.env(),
        timeout=120,
    )
    out = (ran.stdout or "") + (ran.stderr or "")
    assert ran.returncode == 0, out[-3000:]
    issues = harness.read_gh_state()["issues"]
    assert issues[META]["state"] == "closed", (issues[META], out[-3000:])
    assert issues[META]["labels"] == [], issues[META]
    assert issues["201"]["state"] == "open", issues["201"]
    assert issues["201"]["labels"] == [], issues["201"]
    assert issues["201"]["comments"] == [], issues["201"]
    assert issues[META]["sub_issues"] == []


def _followup_world(h: Harness, pr_state: str) -> None:
    feature = _issue(
        "flaky-test: feature-stage follow-up",
        f"x Refs #{META} <!-- grind:followup meta={META} stage=feature feature-pr=150 -->",
    )
    bug = _issue(
        "doc-gap: bug-stage follow-up",
        f"y Refs #{META} <!-- grind:followup meta={META} stage=bugs feature-pr=none -->",
    )
    state = _world({int(META): _issue("meta: backlog", "done"), 201: feature, 202: bug})
    state["prs"] = [
        {
            "number": 150,
            "head": f"grind/meta-{META}-1f3a",
            "state": pr_state,
            "title": "feature",
            "base": "main",
        }
    ]
    h.write_gh_state(state)


def _intake(pr_state: str, pick: str) -> list[dict[str, Any]]:
    """Intake with no argument: check each follow-up's gate, pick the eligible one."""
    return [
        _bash(f"gh issue list --state open --search '\"{MARKER}\" in:body'"),
        _after(
            {"is_error": False, "content_contains": MARKER},
            _bash("gh pr view 150 --json state -q .state"),
        ),
        _after(
            {"is_error": False, "content_contains": pr_state},
            {"text": f"PICKED #{pick}"},
        ),
    ]


def test_x7_intake_gates_followups_on_their_feature_pr(harness: Harness) -> None:
    for name in ("grind-intake", "grind-cron", "clud-issue-triage"):
        text = (harness.config / "skills" / name / "SKILL.md").read_text(encoding="utf-8")
        assert MARKER in text, (name, text[-2000:])
        assert "stage=bugs" in text, (name, text[-2000:])
        assert "feature-pr=" in text, (name, text[-2000:])
        assert "MERGED" in text, (name, text[-2000:])

    # Feature PR still open: only the bug-stage follow-up (#202) is eligible.
    _followup_world(harness, "OPEN")
    first = harness.run(
        "/grind",
        {"default_text": "OK", "roles": [{"name": "main", "steps": _intake("OPEN", "202")}]},
    )
    assert first.returncode == 0, first.stdout[-3000:]
    _no_notes(first)
    assert "PICKED #202" in first.stdout, first.stdout[-3000:]

    # Once the feature PR merged, the feature-stage follow-up (#201) is picked.
    state = harness.read_gh_state()
    state["prs"][0]["state"] = "MERGED"
    harness.write_gh_state(state)
    harness.new_session()
    second = harness.run(
        "/grind",
        {"default_text": "OK", "roles": [{"name": "main", "steps": _intake("MERGED", "201")}]},
    )
    assert second.returncode == 0, second.stdout[-3000:]
    _no_notes(second)
    assert "PICKED #201" in second.stdout, second.stdout[-3000:]
    state = harness.read_gh_state()
    issues = state["issues"]
    # Intake gates on the marker alone: it never labels or edits a follow-up (#1958).
    assert all(i["labels"] == [] for i in issues.values()), issues
    assert not [c for c in state.get("calls", []) if c[:2] == ["issue", "edit"]]
    # Neither follow-up ever became a sub-issue of the meta.
    assert issues[META]["sub_issues"] == []


def test_x8_failed_filing_is_reported_inline(harness: Harness) -> None:
    _seed(harness)
    state = harness.read_gh_state()
    state["faults"] = {"issue create": {"code": 1, "stderr": "gh: HTTP 502"}}
    harness.write_gh_state(state)
    _run_facts(harness, "issue")
    report = (
        f"GRIND_DONE. Problems not filed: {FLAKY['kind']}: {FLAKY['summary']} ({FLAKY['evidence']})"
    )
    router = [_bash(_file_issue(FLAKY)), _after({"is_error": True}, {"text": report})]
    roles = _goal_roles(harness, worker_problems=[FLAKY])
    result = _run(harness, _script(harness, roles, router))
    _no_notes(result)
    _notified(result, FLAKY)
    assert not _followups(harness)
    # The goal still landed: the failed filing did not stop the run.
    prs = harness.read_gh_state()["prs"]
    assert [p["head"] for p in prs if p.get("state") == "MERGED"] == [f"grind/{GOAL}"]
    assert FLAKY["summary"] in result.stdout, result.stdout[-3000:]
    assert "Problems not filed" in result.stdout

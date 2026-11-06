"""The three `/grind` user scenarios of #1392, end to end on the real Claude Code (#1413).

Each test drives one whole run: routing, the single question round, prework,
the bug stage, the feature stage (or its deferral) and Finish. The main
session is scripted, so the router's `gh`/`git` commands are fixed in the
script, and every grind role is scripted too. What the tests guard is the
contract between them: which roles run and in what order, what the hook lets
through, and the final fake GitHub and git state.

1. **Bugs only** (`meta_with_sub_issues(4)`): four unrelated bugs land on
   `main` with `Closes #N`; one worker's problem becomes a `grind:followup`
   issue; the meta issue is never closed by hand and no feature branch exists.
2. **A prompt building a feature, auto merge**: routing turns the prompt into
   meta #6 (the `mixed()` backlog holds #1-#5) with bugs #7, #8 and feature
   parts #9, #10, #11 (#11 depends on #8). The checkout is dirty and is
   stashed up front. Bugs land on `main` first; the feature goals land on the
   feature branch; the draft feature PR is readied and merge-committed, and
   it alone closes #6 and the feature parts. `main` moving is merged in.
3. **A big messy epic, regrouped, the feature left to decide later**
   (`regroupable()` plus two user-made sub-metas and four bugs): the regroup
   rewrites the user's sub-metas in place as F1 (docs, #10) and F2 (cli,
   #11); the user picks F1 and the merge policy "Decide later". Run 1 lands
   the bugs on `main` and F1's goals on `grind/meta-10-<run>`; F1's feature
   PR stays an open draft and F2 is deferred. Run 2, with F1's PR still
   open, is bugs-only and its plan names the waiting PR. The user then
   merges F1's PR, which closes #10 and its children but never the top
   meta #1.

Scenarios 2 and 3 differ from #1392's prose in one scripted detail: the
feature branch and draft PR exist before the workflow starts (the router sets
them up from its own pre-steps), because the workflow is one tool call and
the main session cannot act between its stages. The first feature-stage
integrator merges the updated `main` in instead.

Not covered here, and why:

- Scenario 1's and 3's "reconcile closes the top meta" step: `clud grind
  reconcile` has no top-meta close yet, so the tests only assert that no
  grind role or router command closes it.
- Scenario 3's `grind:on-feature` labels, markers and the run-2 reconcile
  that reopens a hand-closed F1 child: the lander's caps refuse
  `gh issue edit`/`gh issue comment`, so no role can label today;
  `test_grind_reconcile.py` covers reconcile on a labelled world.
- Scenario 3's run 3 (F2): it repeats run 1's machinery for the other group.

Script notes: a step's `expect` checks the tool results of the *previous*
step, so an expectation about a command sits on the step after it. The
`Workflow` tool returns "launched in background" at once, and the workflow's
own result arrives later as a task notification (a user turn, not a tool
result). So the main session ends its turn right after the launch
(`_workflow_steps`), and its Finish steps run on the notification turn;
the tests check the workflow's outcome in the fake GitHub and git state.
"""

from __future__ import annotations

import itertools
import json
import re
import shlex
from pathlib import Path
from typing import Any

from tests import process
from tests.harness.harness import Harness, RunResult
from tests.harness.test_grind_meta_of_metas import PREVIOUS, V1, _regroup
from tests.harness.worlds import _issue, meta_with_sub_issues, mixed, regroupable

MARK = {
    "prework": "You are the /grind prework role",
    "planner": "You are the /grind planner",
    "worker": "You are the /grind editor",
    "reviewer": "You are the /grind reviewer",
    "integrator": "You are the /grind integrator",
    "lander": "You are the /grind lander",
    "chaperone": "You are the /grind chaperone",
}
PLAN_MARKER = "<!-- grind:v1 plan run="
FOLLOWUP = "grind:followup"
OK = {"is_error": False}


def _structured(value: dict[str, Any]) -> dict[str, Any]:
    return {"tool_use": {"name": "StructuredOutput", "input": value}}


def _bash(command: str) -> dict[str, Any]:
    return {"tool_use": {"name": "Bash", "input": {"command": command, "description": "grind"}}}


def _write(path: Path, content: str) -> dict[str, Any]:
    return {"tool_use": {"name": "Write", "input": {"file_path": str(path), "content": content}}}


def _after(expect: dict[str, Any], step: dict[str, Any]) -> dict[str, Any]:
    """`step`, first checking the previous step's tool results."""
    return {**step, "expect": expect}


def _saw(text: str) -> dict[str, Any]:
    return {"is_error": False, "content_contains": text}


def _chain(steps: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Each step after the first expects the previous one to have succeeded,
    unless it already carries its own expectation."""
    return [s if i == 0 or "expect" in s else _after(OK, s) for i, s in enumerate(steps)]


def _role(
    name: str, role: str, prompt: str | list[str], steps: list[dict[str, Any]]
) -> dict[str, Any]:
    return {"name": name, "match": MARK[role], "match_prompt": prompt, "steps": steps}


def _ask(*questions: tuple[str, str, list[str]]) -> dict[str, Any]:
    """One AskUserQuestion call: (question, header, option labels) each."""
    qs = [
        {
            "question": q,
            "header": header,
            "multiSelect": False,
            "options": [{"label": o, "description": o} for o in options],
        }
        for q, header, options in questions
    ]
    return {"tool_use": {"name": "AskUserQuestion", "input": {"questions": qs}}}


def _workflow_steps(args: dict[str, Any]) -> list[dict[str, Any]]:
    """Launch `grind-run`, then end the turn until its completion notification.

    The step after these two runs on the notification turn. Its `expect` can
    only see tool results, and the notification is plain user text, so it
    carries no `content_contains` check.
    """
    return [
        {"tool_use": {"name": "Workflow", "input": {"name": "grind-run", "args": args}}},
        {"text": "grind-run is running in the background; waiting for it to finish"},
    ]


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


def _calls(h: Harness, *head: str) -> list[list[str]]:
    return [c for c in h.read_gh_state().get("calls", []) if c[: len(head)] == list(head)]


def _pr(h: Harness, number: int) -> dict[str, Any]:
    return next(p for p in h.read_gh_state()["prs"] if p["number"] == number)


def _issues(h: Harness) -> dict[str, dict[str, Any]]:
    return h.read_gh_state()["issues"]


def _origin(h: Harness, rev: str) -> str:
    return h.git("rev-parse", rev, cwd=h.origin)


def _origin_heads(h: Harness) -> list[str]:
    out = h.git("for-each-ref", "--format=%(refname:short)", "refs/heads", cwd=h.origin)
    return out.split()


def _run_path(h: Harness) -> Path:
    return h.run_facts_path()


def _write_run(h: Harness, facts: dict[str, Any]) -> None:
    run = _run_path(h)
    run.parent.mkdir(parents=True, exist_ok=True)
    run.write_text(json.dumps(facts), encoding="utf-8")


def _questions_then_none(result: RunResult, asked: int) -> None:
    """`asked` AskUserQuestion calls before prework, and none from anyone after."""
    assert len(result.questions_before("grind-prework")) == asked, result.hooks
    seen_prework = False
    for record in result.hooks:
        if record.get("agent_type") == "grind-prework":
            seen_prework = True
        if seen_prework:
            assert record.get("tool_name") != "AskUserQuestion", record
    assert seen_prework, "prework never ran"


def _all_goal_roles_ran(result: RunResult, goals: list[str]) -> None:
    first = _first(result)
    for g in goals:
        for role in ("planner", "worker", "reviewer", "integrator", "chaperone"):
            assert f"{role}:{g}" in first, (role, g, sorted(first))
        assert first["prework"] < first[f"planner:{g}"], (g, first)
        assert (
            first[f"planner:{g}"]
            < first[f"worker:{g}"]
            < first[f"reviewer:{g}"]
            < first[f"integrator:{g}"]
            < first[f"chaperone:{g}"]
        ), (g, first)


# ---- scripted roles ------------------------------------------------------------


def _prework(meta: str, run_id: str) -> dict[str, Any]:
    url = f"https://github.com/o/r/issues/{meta}#issuecomment-1000"
    steps = [
        _bash(f"gh issue comment {meta} --repo o/r --body '{PLAN_MARKER}{run_id} -->'"),
        _after(OK, _structured({"posted": True, "plan_url": url, "part_urls": [url]})),
    ]
    return _role("prework", "prework", "/grind-prework", steps)


def _bug_roles(
    h: Harness, goal: str, pr: int, *, problems: list[dict[str, Any]] | None = None
) -> list[dict[str, Any]]:
    """One bug goal: worked in the user's checkout, PR into main with `Closes #N`."""
    branch = f"grind/{goal}"
    repo = str(h.repo)
    at = f"Goal {goal}:"
    plan = {
        "checkout": repo,
        "branch": branch,
        "depends_on": [],
        "verify": "true",
        "tasks": [{"id": "t1", "files": [f"{goal}.txt"], "instructions": f"fix {goal}"}],
    }
    write = {"file_path": f"{repo}/{goal}.txt", "content": f"{goal}\n"}
    work: dict[str, Any] = {"files_touched": [f"{goal}.txt"], "summary": "fixed"}
    if problems:
        work["problems"] = problems
    git = f"git -C {repo}"
    push = (
        f"{git} switch -q main && {git} switch -q -c {branch} && "
        f"{git} add {goal}.txt && {git} commit -q -m 'fix: #{goal}' && "
        f"{git} push -q -u origin {branch} && "
        f"gh pr create --title 'fix #{goal}' --head {branch} --base main --body 'Closes #{goal}'"
    )
    url = f"https://github.com/o/r/pull/{pr}"
    return [
        _role(f"planner:{goal}", "planner", at, [_structured(plan)]),
        _role(
            f"worker:{goal}",
            "worker",
            at,
            [
                {"tool_use": {"name": "Write", "input": write}},
                _after(OK, _structured(work)),
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
        _role(
            f"chaperone:{goal}",
            "chaperone",
            [at, "Follow your WATCH"],
            [
                _bash(f"gh pr merge {pr} --squash --match-head-commit HEAD"),
                _after(OK, _structured({"verdict": "merged", "summary": "green"})),
            ],
        ),
    ]


def _goals(ids: list[str]) -> list[dict[str, str]]:
    return [{"id": g, "title": f"child {g}", "brief": f"do {g}"} for g in ids]


# ---- scenario 1: a meta issue of unrelated bugs --------------------------------

S1_META = "1"
S1_BUGS = ["2", "3", "4", "5"]
S1_RUN = "b1a5"
S1_PROBLEM = {
    "kind": "pre-existing-failure",
    "summary": "test_locale_parse fails on untouched main",
    "evidence": "fails on origin/main before any change",
}


def test_e2e_1_bugs_only_each_lands_on_main_and_the_meta_is_not_hand_closed(
    harness: Harness,
) -> None:
    h = harness
    h.write_gh_state(meta_with_sub_issues(4))
    _write_run(h, {"mode": "sequential", "meta": S1_META, "problem_reporting": "issue"})
    prs = {g: 101 + i for i, g in enumerate(S1_BUGS)}
    plan = {
        "schema": "grind-plan/v1",
        "run_id": S1_RUN,
        "meta": int(S1_META),
        "repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "preflight": {"action": "none", "branch": "main"},
        "structure": "simple",
        "stages": [{"stage": "bugs", "base": "main", "children": S1_BUGS}],
        "deferred_groups": [],
        "problem_reporting": "issue",
        "models": {},
        "ci": False,
        "scripts": {},
        "rules": {"stuck_bug": "block_dependents_only", "no_overlap": "bugs_only"},
    }
    args = {
        "repo": str(h.repo),
        "github_repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "meta": S1_META,
        "plan": plan,
        "goals": _goals(S1_BUGS),
    }
    title = shlex.quote(f"{S1_PROBLEM['kind']}: {S1_PROBLEM['summary']}")
    body = shlex.quote(
        f"{S1_PROBLEM['evidence']} Refs #{S1_META} "
        f"<!-- grind:followup meta={S1_META} stage=bugs feature-pr=none -->"
    )
    followup = (
        f"gh issue create --repo o/r --title {title} --label {FOLLOWUP} --body {body}"
    )
    launch, wait = _workflow_steps(args)
    main_steps = _chain(
        [
            # Routing: native sub-issues, so no conversion question.
            _bash(f"gh api repos/o/r/issues/{S1_META}/sub_issues"),
            # The one question round: no merge-policy question (no feature stage).
            _ask(
                ("Run mode?", "Mode", ["Sequential", "Parallel"]),
                ("Where do problems go?", "Problems", ["New issue per problem", "Comment"]),
            ),
            _after(_saw("New issue per problem"), launch),
            wait,
            # Finish, on the workflow's notification turn: file the problem.
            _bash(followup),
            {"text": "GRIND_DONE: 4 bugs merged, 1 follow-up filed"},
        ]
    )
    roles: list[dict[str, Any]] = [_prework(S1_META, S1_RUN)]
    for g in S1_BUGS:
        roles += _bug_roles(h, g, prs[g], problems=[S1_PROBLEM] if g == "3" else None)
    script = {"default_text": "OK", "roles": [*roles, {"name": "main", "steps": main_steps}]}
    answers = {"Run mode?": "Sequential", "Where do problems go?": "New issue per problem"}
    result = h.run("start grind", script, timeout=900, answers=answers)
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)

    # Order: one question round, then prework, then each bug end to end in turn.
    _questions_then_none(result, 1)
    _all_goal_roles_ran(result, S1_BUGS)
    first = _first(result)
    for a, b in itertools.pairwise(S1_BUGS):
        assert first[f"integrator:{a}"] < first[f"planner:{b}"], (a, b, first)

    # Every bug PR goes straight into main with Closes, and its merge closed it.
    issues = _issues(h)
    for g in S1_BUGS:
        pr = _pr(h, prs[g])
        assert pr["head"] == f"grind/{g}", pr
        assert pr["base"] == "main", pr
        assert f"Closes #{g}" in pr["body"], pr
        assert pr["state"] == "MERGED", pr
        assert issues[g]["state"] == "closed", (g, issues[g])
        assert issues[g]["closed_by"] == {"kind": "pr", "pr": prs[g]}, (g, issues[g])

    # No feature machinery at any point.
    state = h.read_gh_state()
    assert not [p for p in state["prs"] if p.get("draft") or p["base"] != "main"], state["prs"]
    assert not [b for b in _origin_heads(h) if b.startswith("grind/meta-")], _origin_heads(h)
    assert not (Path(h.repo) / ".clud" / "grind" / "worktrees" / "feature").exists()

    # The meta issue is left for reconcile: open, never closed by a command.
    assert issues[S1_META]["state"] == "open", issues[S1_META]
    assert not _calls(h, "issue", "close"), state["calls"]
    plan_comments = [c for c in issues[S1_META]["comments"] if PLAN_MARKER in c["body"]]
    assert len(plan_comments) == 1, issues[S1_META]["comments"]

    # The problem became exactly one follow-up issue, never a sub-issue.
    followups = {n: i for n, i in issues.items() if FOLLOWUP in i.get("labels", [])}
    assert list(followups) == ["105"], followups
    assert f"Refs #{S1_META}" in followups["105"]["body"]
    assert followups["105"]["parent"] is None
    assert 105 not in [s["number"] for s in issues[S1_META]["sub_issues"]]
    assert len(_calls(h, "issue", "create")) == 1


# ---- scenario 2: a prompt, prerequisite bugs, a dirty checkout, auto merge -----

S2_META = "6"
S2_BUGS = ["7", "8"]
S2_FEATURES = ["9", "10", "11"]
S2_RUN = "2b7c"
S2_FEATURE = f"grind/meta-{S2_META}-{S2_RUN}"
S2_FEATURE_PR = 101
S2_STASH = f"grind-{S2_RUN}"
S2_PROBLEM = {
    "kind": "doc-gap",
    "summary": "OAuth callback URL is undocumented",
    "evidence": "README has no callback section",
    "related_issue": "9",
}
S2_CHILDREN = [
    ("7", "fix: session cookie is not refreshed"),
    ("8", "fix: login redirect drops the return URL"),
    ("9", "feat: OAuth provider config"),
    ("10", "feat: OAuth callback endpoint"),
    ("11", "feat: OAuth login button (needs the redirect fix)"),
]


def _wt(h: Harness) -> Path:
    return Path(h.repo) / ".clud" / "grind" / "worktrees" / "feature"


def _closes(meta: str, goals: list[str], goal: str) -> str:
    """The feature PR body once `goal` (and every goal before it) has landed."""
    done = goals[: goals.index(goal) + 1]
    return " ".join(f"Closes #{n}" for n in [meta, *done])


def _feature_roles(
    h: Harness,
    goal: str,
    pr: int,
    *,
    feature: str,
    feature_pr: int,
    closes: str,
    merge_main: bool = False,
    problems: list[dict[str, Any]] | None = None,
) -> list[dict[str, Any]]:
    """One feature goal, worked in the feature worktree, PR into the feature branch."""
    branch = f"grind/{goal}"
    at = f"Goal {goal}:"
    checkout = str(_wt(h))
    plan = {
        "checkout": checkout,
        "branch": branch,
        "depends_on": [],
        "verify": "true",
        "tasks": [{"id": "t1", "files": [f"{goal}.txt"], "instructions": f"build {goal}"}],
    }
    write = {"file_path": f"{checkout}/{goal}.txt", "content": f"{goal}\n"}
    work: dict[str, Any] = {"files_touched": [f"{goal}.txt"], "summary": "built"}
    if problems:
        work["problems"] = problems
    git = f"git -C {checkout}"
    # One GitHub/remote mutation per command (#1979): only `git push && gh pr
    # create` may share one, so merging main in is its own step.
    integrate: list[dict[str, Any]] = []
    if merge_main:
        integrate.append(
            _bash(
                f"{git} fetch -q origin && "
                f"{git} merge -q --no-ff origin/main -m 'merge main into {feature}' && "
                f"{git} push -q origin HEAD:{feature}"
            )
        )
    steps = [
        f"{git} fetch -q origin",
        f"{git} switch -q -c {branch} origin/{feature}",
        f"{git} add {goal}.txt",
        f"{git} commit -q -m 'feat: #{goal}'",
        f"{git} push -q -u origin {branch}",
        f"gh pr create --title 'feat #{goal}' --head {branch} --base {feature} "
        f"--body 'Refs #{goal}'",
    ]
    integrate.append(_bash(" && ".join(steps)))
    url = f"https://github.com/o/r/pull/{pr}"
    return [
        _role(f"planner:{goal}", "planner", at, [_structured(plan)]),
        _role(
            f"worker:{goal}",
            "worker",
            at,
            [
                {"tool_use": {"name": "Write", "input": write}},
                _after(OK, _structured(work)),
            ],
        ),
        _role(
            f"reviewer:{goal}", "reviewer", at, [_structured({"approved": True, "summary": "ok"})]
        ),
        _role(
            f"integrator:{goal}",
            "integrator",
            at,
            _chain(
                [
                    *integrate,
                    _structured({"pushed": True, "pr_url": url, "summary": "p"}),
                ]
            ),
        ),
        _role(
            f"chaperone:{goal}",
            "chaperone",
            [at, "Follow your WATCH"],
            # The lander, not the integrator, owns the feature PR body (#1393 §2).
            _chain(
                [
                    _bash(f"gh pr merge {pr} --merge"),
                    _bash(f"gh pr edit {feature_pr} --body '{closes}'"),
                    _structured({"verdict": "merged", "summary": "green"}),
                ]
            ),
        ),
    ]


def _feature_lander() -> dict[str, Any]:
    steps = [
        _bash(f"gh pr ready {S2_FEATURE_PR}"),
        _after(OK, _bash(f"gh pr merge {S2_FEATURE_PR} --merge")),
        _after(OK, _structured({"status": "merged", "summary": "feature merged"})),
    ]
    return _role("lander:feature", "lander", "for the FEATURE PR", steps)


def test_e2e_2_prompt_feature_auto_merge_bugs_first_then_one_feature_merge_commit(
    harness: Harness,
) -> None:
    h = harness
    h.write_gh_state(mixed())
    before = {n: dict(i) for n, i in h.read_gh_state()["issues"].items()}
    # The feature worktree lives inside the repo; keep it out of
    # the stash, as a real repo's ignore rules would.
    exclude = Path(h.repo) / ".git" / "info" / "exclude"
    exclude.write_text(exclude.read_text(encoding="utf-8") + ".clud/\n", encoding="utf-8")
    feature_before = h.git("rev-parse", "main")
    h.git("branch", S2_FEATURE, "main")
    h.git("push", "-q", "-u", "origin", S2_FEATURE)
    h.git("worktree", "add", "-q", str(_wt(h)), S2_FEATURE)
    # main moves after the feature branch was cut.
    h.git("commit", "-q", "--allow-empty", "-m", "main moved")
    h.git("push", "-q", "origin", "main")
    main_after = h.git("rev-parse", "main")
    # A dirty checkout: one modified tracked file, one untracked file.
    readme = Path(h.repo) / "README.md"
    readme.write_text(readme.read_text(encoding="utf-8") + "my local edit\n", encoding="utf-8")
    notes = Path(h.repo) / "notes.txt"
    notes.write_text("scratch\n", encoding="utf-8")
    _write_run(
        h,
        {
            "mode": "sequential",
            "meta": S2_META,
            "feature": {
                "branch": S2_FEATURE,
                "worktree": str(_wt(h)),
                "pr": f"https://github.com/o/r/pull/{S2_FEATURE_PR}",
            },
            "feature_merge": "auto",
            "problem_reporting": "comment",
            "preflight": {"action": "stash", "stash": S2_STASH, "branch": "main"},
        },
    )
    bug_prs = {"7": 102, "8": 103}
    feature_prs = {"9": 104, "10": 105, "11": 106}
    plan = {
        "schema": "grind-plan/v1",
        "run_id": S2_RUN,
        "meta": int(S2_META),
        "repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "preflight": {"action": "stash", "stash": S2_STASH, "branch": "main"},
        "structure": "simple",
        "stages": [
            {"stage": "bugs", "base": "main", "children": S2_BUGS},
            {
                "stage": "feature",
                "group": "oauth login",
                "sub_meta": None,
                "branch": S2_FEATURE,
                "base": S2_FEATURE,
                "children": S2_FEATURES,
                "depends_on_bugs": {"11": ["8"]},
            },
        ],
        "deferred_groups": [],
        "feature_merge": "auto",
        "problem_reporting": "comment",
        "models": {},
        "ci": False,
        "scripts": {},
        "rules": {"stuck_bug": "block_dependents_only", "no_overlap": "bugs_only"},
    }
    args = {
        "repo": str(h.repo),
        "github_repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "meta": S2_META,
        "plan": plan,
        "goals": [{"id": n, "title": t, "brief": t} for n, t in S2_CHILDREN],
        "feature": {
            "branch": S2_FEATURE,
            "worktree": str(_wt(h)),
            "pr": f"https://github.com/o/r/pull/{S2_FEATURE_PR}",
        },
        "feature_merge": "auto",
    }
    # Routing: the prompt becomes meta #6 and five children, attached and verified.
    route = [
        "gh issue create --repo o/r --title 'meta: OAuth login' --body 'Tracked as sub-issues.'",
        *[
            f"gh issue create --repo o/r --title {shlex.quote(t)} --body {shlex.quote(t)}"
            for _, t in S2_CHILDREN
        ],
        *[
            f"gh api -X POST repos/o/r/issues/{S2_META}/sub_issues -F sub_issue_id={n}"
            for n, _ in S2_CHILDREN
        ],
        f"gh api repos/o/r/issues/{S2_META}/sub_issues",
    ]
    repo = str(h.repo)
    draft = (
        f"gh pr create --draft --title 'grind: meta {S2_META}' --head {S2_FEATURE} "
        f"--base main --body 'Closes #{S2_META}'"
    )
    comment = "gh issue comment 9 --body " + shlex.quote(
        f"grind problem ({S2_PROBLEM['kind']}): {S2_PROBLEM['summary']}. {S2_PROBLEM['evidence']}"
    )
    main_steps = _chain(
        [
            # One GitHub mutation per command (#1979).
            *[_bash(command) for command in route],
            _ask(
                (
                    "Your checkout has uncommitted changes. What should grind do?",
                    "Dirty repo",
                    [
                        "Stash it",
                        "Commit to a WIP branch",
                        "Carry into the grind worktree",
                        "Abort",
                    ],
                ),
                (
                    "What happens to the feature PR when every feature goal has landed?",
                    "Merge",
                    ["Auto-merge when done (Recommended)", "Decide later", "Comment only"],
                ),
                ("Where do problems go?", "Problems", ["New issue per problem", "Comment"]),
            ),
            _after(
                _saw("Stash it"), _bash(f"git -C {repo} stash push -q -u -m {S2_STASH}")
            ),
            _bash(draft),
            *_workflow_steps(args),
            # Finish, on the workflow's notification turn: the problem comment.
            _bash(comment),
            # Finish: back to the starting branch, stash restored.
            _bash(f"git -C {repo} switch -q main && git -C {repo} stash pop -q"),
            {"text": "GRIND_DONE: 2 bugs merged, feature PR merged"},
        ]
    )
    roles: list[dict[str, Any]] = [_prework(S2_META, S2_RUN), _feature_lander()]
    for g in S2_BUGS:
        roles += _bug_roles(h, g, bug_prs[g])
    for g in S2_FEATURES:
        roles += _feature_roles(
            h,
            g,
            feature_prs[g],
            feature=S2_FEATURE,
            feature_pr=S2_FEATURE_PR,
            closes=_closes(S2_META, S2_FEATURES, g),
            merge_main=g == S2_FEATURES[0],
            problems=[S2_PROBLEM] if g == S2_PROBLEM["related_issue"] else None,
        )
    script = {"default_text": "OK", "roles": [*roles, {"name": "main", "steps": main_steps}]}
    answers = {
        "Your checkout has uncommitted changes. What should grind do?": "Stash it",
        "What happens to the feature PR when every feature goal has landed?": (
            "Auto-merge when done (Recommended)"
        ),
        "Where do problems go?": "Comment",
    }
    result = h.run(
        "/grind add OAuth login; the session cookie bug and the redirect bug must be fixed first",
        script,
        timeout=1200,
        answers=answers,
    )
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)

    # Routing made meta #6 with five native sub-issues; the backlog is untouched.
    issues = _issues(h)
    assert {s["number"] for s in issues[S2_META]["sub_issues"]} == {7, 8, 9, 10, 11}
    assert all(issues[n]["parent"] == int(S2_META) for n, _ in S2_CHILDREN)
    for n, old in before.items():
        assert (issues[n]["title"], issues[n]["body"]) == (old["title"], old["body"]), n

    # Order: questions, prework, every bug landed, then the feature goals, then
    # the feature PR.
    _questions_then_none(result, 1)
    _all_goal_roles_ran(result, [*S2_BUGS, *S2_FEATURES])
    first = _first(result)
    last_bug = max(first[f"chaperone:{b}"] for b in S2_BUGS)
    assert all(last_bug < first[f"planner:{f}"] for f in S2_FEATURES), first
    assert first["lander:feature"] > max(first[f"chaperone:{f}"] for f in S2_FEATURES), first
    calls = h.read_gh_state()["calls"]
    drafted = next(i for i, c in enumerate(calls) if c[:2] == ["pr", "create"] and "--draft" in c)
    merged = next(i for i, c in enumerate(calls) if c[:2] == ["pr", "merge"])
    assert drafted < merged, calls

    # Bug stage: PRs into main, each closing its own issue.
    for g in S2_BUGS:
        pr = _pr(h, bug_prs[g])
        assert pr["base"] == "main", pr
        assert f"Closes #{g}" in pr["body"], pr
        assert pr["state"] == "MERGED", pr
        assert issues[g]["closed_by"] == {"kind": "pr", "pr": bug_prs[g]}, (g, issues[g])
        assert "Base: origin/main" in _prompt(result, f"integrator:{g}")

    # Feature stage: goal PRs into the feature branch with Refs, never Closes.
    for g in S2_FEATURES:
        pr = _pr(h, feature_prs[g])
        assert pr["base"] == S2_FEATURE, pr
        assert pr["state"] == "MERGED", pr
        assert f"Refs #{g}" in pr["body"], pr
        assert "Closes" not in pr["body"], pr
        assert f"Base: origin/{S2_FEATURE}" in _prompt(result, f"integrator:{g}")

    # The feature PR: readied, merge-committed without --admin, and the only
    # closer of the meta issue and every feature part.
    feature = _pr(h, S2_FEATURE_PR)
    assert feature["base"] == "main", feature
    assert feature["state"] == "MERGED", feature
    assert feature["draft"] is False, feature
    assert feature["merge_method"] == "merge", feature
    assert feature["admin"] is False, feature
    ready = _calls(h, "pr", "ready", str(S2_FEATURE_PR))
    merge = _calls(h, "pr", "merge", str(S2_FEATURE_PR))
    assert ready, calls
    assert merge, calls
    assert calls.index(ready[0]) < calls.index(merge[0]), calls
    assert all("--admin" not in c and "--squash" not in c for c in merge), merge
    for n in (S2_META, *S2_FEATURES):
        assert f"Closes #{n}" in feature["body"], feature["body"]
        assert issues[n]["state"] == "closed", (n, issues[n])
        assert issues[n]["closed_by"] == {"kind": "pr", "pr": S2_FEATURE_PR}, (n, issues[n])
    assert not _calls(h, "issue", "close"), calls

    # main moving was merged into the feature branch, never rebased.
    assert _origin(h, "main") == main_after
    tip = _origin(h, S2_FEATURE)
    parents = h.git("rev-list", "--parents", "-n", "1", tip, cwd=h.origin).split()
    assert parents[1:] == [feature_before, main_after], parents

    # Problems went where the user chose: one comment on the related child.
    assert not [i for i in issues.values() if FOLLOWUP in i.get("labels", [])]
    on_child = [c for c in issues["9"]["comments"] if S2_PROBLEM["summary"] in c["body"]]
    assert len(on_child) == 1, issues["9"]["comments"]

    # The public plan leaves out local-only details.
    pre = _prompt(result, "prework")
    assert '"preflight": "handled"' in pre, pre[-3000:]
    assert S2_STASH not in pre

    # Finish restored the stash and the starting branch.
    assert h.git("branch", "--show-current") == "main"
    assert h.git("stash", "list") == ""
    assert "my local edit" in readme.read_text(encoding="utf-8")
    assert notes.read_text(encoding="utf-8") == "scratch\n"


# ---- scenario 3: a big messy epic, regrouped, feature decided later ------------

S3_TOP = "1"
S3_RUN = "e3d4"
S3_RUN2 = "e3d5"
S3_BUGS = ["12", "13", "14", "15"]
S3_BUG2 = "16"
S3_DOCS = ["2", "3", "4", "5"]
S3_CLI = ["6", "7", "8", "9"]
S3_F1 = "10"
S3_F2 = "11"
S3_FEATURE = f"grind/meta-{S3_F1}-{S3_RUN}"
S3_FEATURE_PR = 101
S3_GROUPS = [("docs", [int(n) for n in S3_DOCS]), ("cli", [int(n) for n in S3_CLI])]
S3_REGROUP_Q = (
    "Regroup #1 into a meta of metas? bugs: #12 #13 #14 #15 "
    "· F1 docs: #2 #3 #4 #5 · F2 cli: #6 #7 #8 #9"
)
S3_PICK_Q = "Which feature does this run do?"
S3_MERGE_Q = "What happens to the feature PR when every feature goal has landed?"
S3_PROBLEMS_Q = "Where do problems go?"
S3_REPORT = (
    f"GRIND_DONE: 4 bugs merged; F1 docs landed on {S3_FEATURE}, feature PR "
    f"#{S3_FEATURE_PR} left open for you; F2 cli deferred"
)
S3_REPORT2 = f"GRIND_DONE: 1 bug merged; feature PR #{S3_FEATURE_PR} for #1 is still open"


def _move(state: dict[str, Any], child: int, parent: int) -> None:
    """Reparent `child` under `parent`, both directions kept consistent."""
    issues = state["issues"]
    old = issues[str(child)]["parent"]
    if old is not None:
        subs = issues[str(old)]["sub_issues"]
        issues[str(old)]["sub_issues"] = [s for s in subs if s["number"] != child]
    issues[str(child)]["parent"] = parent
    issues[str(parent)]["sub_issues"].append({"number": child, "state": "open"})


def _messy_epic() -> dict[str, Any]:
    """`regroupable()` (docs #2-#5, cli #6-#9 under #1), plus the user's own
    sub-metas #10 (holding #2, #3) and #11 (holding #6, #7), and bugs #12-#15."""
    state = regroupable()
    issues = state["issues"]
    issues[S3_F1] = _issue("meta: my docs notes", "Docs things I noticed.", parent=1)
    issues[S3_F2] = _issue("meta: my cli notes", "CLI things I noticed.", parent=1)
    for n in (S3_F1, S3_F2):
        issues[S3_TOP]["sub_issues"].append({"number": int(n), "state": "open"})
    for child, parent in ((2, 10), (3, 10), (6, 11), (7, 11)):
        _move(state, child, parent)
    for n in S3_BUGS:
        issues[n] = _issue(f"bug {n}", f"fix {n}", parent=1)
        issues[S3_TOP]["sub_issues"].append({"number": int(n), "state": "open"})
    return state


def _s3_plan(run_id: str, stages: list[dict[str, Any]], **extra: Any) -> dict[str, Any]:
    return {
        "schema": "grind-plan/v1",
        "run_id": run_id,
        "meta": int(S3_TOP),
        "repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "preflight": {"action": "none", "branch": "main"},
        "structure": "meta_of_metas",
        "stages": stages,
        "problem_reporting": "issue",
        "models": {},
        "ci": False,
        "scripts": {},
        "rules": {"stuck_bug": "block_dependents_only", "no_overlap": "bugs_only"},
        **extra,
    }


def _saw_goal(result: RunResult, goal: str) -> list[str]:
    """The scripted roles (other than main) whose conversation named `goal`."""
    return [
        r["role"]
        for r in result.requests
        if r["role"] != "main" and f"Goal {goal}:" in _text(r.get("messages"))
    ]


def _user_merges(h: Harness, pr: int) -> None:
    """The user readies and merges `pr` on GitHub (through `fake_gh`)."""
    for sub in (["pr", "ready", str(pr)], ["pr", "merge", str(pr), "--merge"]):
        done = process.run(
            [str(h.bin / "gh"), *sub], capture_output=True, text=True, env=h.env(), timeout=60
        )
        assert done.returncode == 0, (sub, done.stdout, done.stderr)


def test_e2e_3_regrouped_epic_decide_later_one_feature_then_bugs_only(  # noqa: C901
    harness: Harness,
) -> None:
    h = harness
    h.write_gh_state(_messy_epic())
    before = h.read_gh_state()
    cmds, undo, where = _regroup(before, S3_GROUPS)
    # The user's two sub-metas are reused in place as F1 and F2; nothing new.
    assert where == {"docs": int(S3_F1), "cli": int(S3_F2)}, where
    # The router's feature setup for F1 (see the module docstring): the
    # branch is named for the group's sub-meta, in the feature worktree, which
    # a real repo's ignore rules keep out of the checkout's status.
    exclude = Path(h.repo) / ".git" / "info" / "exclude"
    exclude.write_text(exclude.read_text(encoding="utf-8") + ".clud/\n", encoding="utf-8")
    h.git("branch", S3_FEATURE, "main")
    h.git("push", "-q", "-u", "origin", S3_FEATURE)
    h.git("worktree", "add", "-q", str(_wt(h)), S3_FEATURE)
    feature = {
        "branch": S3_FEATURE,
        "worktree": str(_wt(h)),
        "pr": f"https://github.com/o/r/pull/{S3_FEATURE_PR}",
    }
    bug_prs = {g: 102 + i for i, g in enumerate(S3_BUGS)}
    docs_prs = {g: 106 + i for i, g in enumerate(S3_DOCS)}
    plan = _s3_plan(
        S3_RUN,
        [
            {"stage": "bugs", "base": "main", "children": S3_BUGS},
            {
                "stage": "feature",
                "group": "docs",
                "sub_meta": int(S3_F1),
                "branch": S3_FEATURE,
                "base": S3_FEATURE,
                "children": S3_DOCS,
                "depends_on_bugs": {},
            },
        ],
        # One feature stage per run; F2 waits, with no branch.
        deferred_groups=[{"group": "cli", "sub_meta": int(S3_F2), "children": S3_CLI}],
        feature_merge="later",
    )
    run = {
        "mode": "sequential",
        "meta": S3_TOP,
        "undo": undo,
        "problem_reporting": "issue",
        "feature_merge": "later",
        "feature": feature,
        "tracks": {**{g: "bug" for g in S3_BUGS}, **{g: "feature" for g in S3_DOCS + S3_CLI}},
        "waiting_on_pr": None,
    }
    # The deferred group's children stay out of this run's goals.
    args = {
        "repo": str(h.repo),
        "github_repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "meta": S3_TOP,
        "plan": plan,
        "goals": _goals([*S3_BUGS, *S3_DOCS]),
        "feature": feature,
        "feature_merge": "later",
    }
    draft = (
        f"gh pr create --draft --title 'grind: docs' --head {S3_FEATURE} "
        f"--base main --body 'Closes #{S3_F1}'"
    )
    main_steps = _chain(
        [
            # Routing: native sub-issues, so no conversion question.
            _bash(f"gh api repos/o/r/issues/{S3_TOP}/sub_issues"),
            # The one question round: regroup, feature pick, merge policy, problems.
            _ask(
                (S3_REGROUP_Q, "Regroup", ["Regroup", "Keep as is (simple schedule)"]),
                (S3_PICK_Q, "Feature", ["docs (Recommended)", "cli"]),
                (
                    S3_MERGE_Q,
                    "Merge",
                    ["Auto-merge when done (Recommended)", "Decide later", "Comment only"],
                ),
                (S3_PROBLEMS_Q, "Problems", ["New issue per problem", "Comment"]),
            ),
            _after(_saw("Decide later"), _write(_run_path(h), json.dumps(run, indent=1))),
            *[_bash(c) for c in cmds],
            _bash(draft),
            *_workflow_steps(args),
            {"text": S3_REPORT},
        ]
    )
    roles: list[dict[str, Any]] = [_prework(S3_TOP, S3_RUN)]
    for g in S3_BUGS:
        roles += _bug_roles(h, g, bug_prs[g])
    for g in S3_DOCS:
        roles += _feature_roles(
            h,
            g,
            docs_prs[g],
            feature=S3_FEATURE,
            feature_pr=S3_FEATURE_PR,
            closes=_closes(S3_F1, S3_DOCS, g),
        )
    script = {"default_text": "OK", "roles": [*roles, {"name": "main", "steps": main_steps}]}
    answers = {
        S3_REGROUP_Q: "Regroup",
        S3_PICK_Q: "docs (Recommended)",
        S3_MERGE_Q: "Decide later",
        S3_PROBLEMS_Q: "New issue per problem",
    }
    result = h.run("start grind", script, timeout=1200, answers=answers)
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)

    # Order: one question round before prework, then the bugs, then F1.
    _questions_then_none(result, 1)
    _all_goal_roles_ran(result, [*S3_BUGS, *S3_DOCS])
    first = _first(result)
    last_bug = max(first[f"chaperone:{b}"] for b in S3_BUGS)
    assert all(last_bug < first[f"planner:{d}"] for d in S3_DOCS), first
    after = h.read_gh_state()
    issues = after["issues"]

    # Regrouped in place: the user's sub-metas became F1/F2, old text kept.
    assert not _calls(h, "issue", "create"), after["calls"]
    for n, name in ((S3_F1, "docs"), (S3_F2, "cli")):
        issue = issues[n]
        assert issue["title"] == f"grind: {name}", issue
        assert issue["body"].startswith(V1), issue
        old = before["issues"][n]["body"]
        assert f"{PREVIOUS}\n\n{old}\n\n</details>" in issue["body"], issue
    assert {s["number"] for s in issues[S3_F1]["sub_issues"]} == {2, 3, 4, 5}
    assert {s["number"] for s in issues[S3_F2]["sub_issues"]} == {6, 7, 8, 9}
    assert {s["number"] for s in issues[S3_TOP]["sub_issues"]} == {10, 11, 12, 13, 14, 15}
    saved = json.loads(_run_path(h).read_text(encoding="utf-8"))["undo"]
    assert {(u["op"], u["issue"]) for u in saved} >= {
        ("rewrite", 10),
        ("rewrite", 11),
        ("reparent", 4),
        ("reparent", 5),
        ("reparent", 8),
        ("reparent", 9),
    }, saved

    # Bugs: each landed on main, closed by its own PR, still under the top meta.
    for g in S3_BUGS:
        pr = _pr(h, bug_prs[g])
        assert pr["base"] == "main", pr
        assert f"Closes #{g}" in pr["body"], pr
        assert pr["state"] == "MERGED", pr
        assert issues[g]["closed_by"] == {"kind": "pr", "pr": bug_prs[g]}, (g, issues[g])
        assert issues[g]["parent"] == int(S3_TOP)

    # F1: goal PRs merged into F1's branch with Refs, so their issues stay open.
    for g in S3_DOCS:
        pr = _pr(h, docs_prs[g])
        assert pr["base"] == S3_FEATURE, pr
        assert pr["state"] == "MERGED", pr
        assert f"Refs #{g}" in pr["body"], pr
        assert "Closes" not in pr["body"], pr
        assert issues[g]["state"] == "open", (g, issues[g])
        assert f"Base: origin/{S3_FEATURE}" in _prompt(result, f"integrator:{g}")

    # "Decide later": F1's feature PR is left open as a draft, never readied or
    # merged by grind. It closes F1's sub-meta and children, never the top meta.
    fpr = _pr(h, S3_FEATURE_PR)
    assert fpr["base"] == "main", fpr
    assert fpr["state"] == "OPEN", fpr
    assert fpr["draft"] is True, fpr
    for n in (S3_F1, *S3_DOCS):
        assert re.search(rf"Closes #{n}(?!\d)", fpr["body"]), fpr["body"]
    assert not re.search(rf"Closes #{S3_TOP}(?!\d)", fpr["body"]), fpr["body"]
    assert not _calls(h, "pr", "ready"), after["calls"]
    assert not _calls(h, "pr", "merge", str(S3_FEATURE_PR)), after["calls"]
    assert issues[S3_F1]["state"] == "open", issues[S3_F1]

    # F2 was deferred: no agent saw its goals, no branch, no PR, still open.
    for g in S3_CLI:
        assert not _saw_goal(result, g), (g, _saw_goal(result, g))
        assert issues[g]["state"] == "open", (g, issues[g])
    assert [b for b in _origin_heads(h) if b.startswith("grind/meta-")] == [S3_FEATURE]
    assert {p["number"] for p in after["prs"]} == {
        S3_FEATURE_PR,
        *bug_prs.values(),
        *docs_prs.values(),
    }, after["prs"]

    # The top meta issue carries the plan and is left open; nothing closed it.
    assert issues[S3_TOP]["state"] == "open"
    assert not _calls(h, "issue", "close"), after["calls"]
    plans = [c for c in issues[S3_TOP]["comments"] if PLAN_MARKER in c["body"]]
    assert len(plans) == 1, issues[S3_TOP]["comments"]

    # ---- run 2, the next day: a new bug, F1's PR still open -> bugs only. ----
    state = h.read_gh_state()
    state["issues"][S3_BUG2] = _issue(f"bug {S3_BUG2}", f"fix {S3_BUG2}", parent=int(S3_TOP))
    state["issues"][S3_TOP]["sub_issues"].append({"number": int(S3_BUG2), "state": "open"})
    h.write_gh_state(state)
    edits_before = len(_calls(h, "issue", "edit"))
    sub_metas_before = {n: dict(state["issues"][n]) for n in (S3_F1, S3_F2)}
    bug2_pr = 110
    plan2 = _s3_plan(
        S3_RUN2,
        [{"stage": "bugs", "base": "main", "children": [S3_BUG2]}],
        deferred_groups=[{"group": "cli", "sub_meta": int(S3_F2), "children": S3_CLI}],
        waiting_on_pr=S3_FEATURE_PR,
    )
    run2 = {
        "mode": "sequential",
        "meta": S3_TOP,
        "undo": [],
        "problem_reporting": "issue",
        "tracks": {S3_BUG2: "bug"},
        "waiting_on_pr": S3_FEATURE_PR,
    }
    args2 = {
        "repo": str(h.repo),
        "github_repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "meta": S3_TOP,
        "plan": plan2,
        "goals": _goals([S3_BUG2]),
    }
    # A second `/grind` is a second Claude Code session, with its own facts.
    h.new_session()
    main_steps2 = _chain(
        [
            # Reconcile first; nothing is labelled, so it has nothing to repair.
            _bash("clud grind reconcile"),
            # No overlap: an open feature PR under #1 makes this run bugs-only.
            _bash("gh pr list --state open --json number,headRefName,isDraft"),
            # Grind-made sub-metas are kept, so there is no regroup question and,
            # with no feature stage, no merge-policy question.
            _after(
                _saw(S3_FEATURE),
                _ask((S3_PROBLEMS_Q, "Problems", ["New issue per problem", "Comment"])),
            ),
            _after(
                _saw("New issue per problem"), _write(_run_path(h), json.dumps(run2, indent=1))
            ),
            *_workflow_steps(args2),
            {"text": S3_REPORT2},
        ]
    )
    roles2: list[dict[str, Any]] = [_prework(S3_TOP, S3_RUN2), *_bug_roles(h, S3_BUG2, bug2_pr)]
    script2 = {"default_text": "OK", "roles": [*roles2, {"name": "main", "steps": main_steps2}]}
    result2 = h.run(
        "start grind",
        script2,
        timeout=900,
        answers={S3_PROBLEMS_Q: "New issue per problem"},
    )
    assert result2.returncode == 0, result2.stdout[-3000:]
    _no_notes(result2)
    _questions_then_none(result2, 1)
    _all_goal_roles_ran(result2, [S3_BUG2])

    after2 = h.read_gh_state()
    issues2 = after2["issues"]
    # The plan records the rule and names the waiting PR.
    pre2 = _prompt(result2, "prework")
    assert f'"waiting_on_pr": {S3_FEATURE_PR}' in pre2, pre2[-3000:]
    assert '"no_overlap": "bugs_only"' in pre2, pre2[-3000:]
    # Only the new bug ran, straight into main.
    pr2 = _pr(h, bug2_pr)
    assert (pr2["base"], pr2["state"]) == ("main", "MERGED"), pr2
    assert issues2[S3_BUG2]["closed_by"] == {"kind": "pr", "pr": bug2_pr}, issues2[S3_BUG2]
    for g in [*S3_DOCS, *S3_CLI]:
        assert not _saw_goal(result2, g), (g, _saw_goal(result2, g))
    assert {p["number"] for p in after2["prs"]} == {p["number"] for p in after["prs"]} | {bug2_pr}
    assert [b for b in _origin_heads(h) if b.startswith("grind/meta-")] == [S3_FEATURE]
    # F1's PR is untouched, and grind's own sub-metas were not regrouped again.
    assert _pr(h, S3_FEATURE_PR) == fpr
    assert len(_calls(h, "issue", "edit")) == edits_before, after2["calls"]
    for n in (S3_F1, S3_F2):
        assert issues2[n]["title"] == sub_metas_before[n]["title"], issues2[n]
        assert issues2[n]["body"] == sub_metas_before[n]["body"], issues2[n]
        assert issues2[n]["sub_issues"] == sub_metas_before[n]["sub_issues"], issues2[n]
    plans2 = [c for c in issues2[S3_TOP]["comments"] if PLAN_MARKER in c["body"]]
    assert len(plans2) == 2, issues2[S3_TOP]["comments"]

    # ---- the user merges F1's PR: GitHub closes F1 and its children. ----
    _user_merges(h, S3_FEATURE_PR)
    final = _issues(h)
    for n in (S3_F1, *S3_DOCS):
        assert final[n]["state"] == "closed", (n, final[n])
        assert final[n]["closed_by"] == {"kind": "pr", "pr": S3_FEATURE_PR}, (n, final[n])
    # The top meta and F2 stay open: no feature PR names the top meta.
    for n in (S3_TOP, S3_F2, *S3_CLI):
        assert final[n]["state"] == "open", (n, final[n])
    assert not _calls(h, "issue", "close"), h.read_gh_state()["calls"]

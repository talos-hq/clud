"""Plan first, then one question round, then nothing asked (#1407, #1392 U1, U3-U14).

The main session plays the `/grind` router: the routing tool, the plan-only
`grind-run` pass, the read-only preflight, the question round (at most two
`AskUserQuestion` calls of at most four questions each), the preflight
action, the run facts, the real run (prework first) and Finish. Every role is
scripted, so these tests pin the *contract*: which role ran in what order,
what was asked and when, what the hook allowed, and the resulting git,
the run facts and fake GitHub state. U2 (plan-only caps) lives in
`test_grind_plan_only.py`.

Script notes:

- A step's `expect` checks the tool results of the *previous* step, so an
  expectation about a call sits on the step after it.
- The Workflow tool returns "Workflow launched in background" at once; its
  result arrives later as a task-notification user turn. So a Workflow step
  is followed by a `_WAIT` text step that ends the turn, and the router's
  next step runs on the notification turn, with no `expect` (the previous
  step was text).
- `AskUserQuestion` is offered in print mode only because every test passes
  `answers=`; the harness answers each question from that table, keyed here
  by header.
- Grind agents are never offered `AskUserQuestion` (their `tools:` lists
  leave it out); clud's hook denying it to every `grind-*` role is the second
  layer, unit-tested in `block_bad_cmd_grind_caps.rs` and `block_bad_cmd.rs`.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from tests.harness.harness import Harness, RunResult
from tests.harness.worlds import _issue, _world, regroupable

MARK = {
    "prework": "You are the /grind prework role",
    "planner": "You are the /grind planner",
    "worker": "You are the /grind editor",
    "reviewer": "You are the /grind reviewer",
}
TOOL = "clud tool run github/is_meta_issue.py"
OK = {"is_error": False}
_WAIT = {"text": "Waiting for the grind-run workflow to finish."}
EXCLUDE = "':(exclude).clud/grind'"

DIRTY_OPTS = ["Stash it", "Commit to a WIP branch", "Carry into the grind worktree", "Abort"]
MERGE_OPTS = ["Auto-merge when done (Recommended)", "Decide later", "Comment only"]
PROBLEM_OPTS = ["New issue per problem (Recommended)", "Comment"]


# ---- script pieces ---------------------------------------------------------------


def _bash(command: str) -> dict[str, Any]:
    return {"tool_use": {"name": "Bash", "input": {"command": command, "description": "grind"}}}


def _write(path: Path, value: dict[str, Any]) -> dict[str, Any]:
    content = json.dumps(value)
    return {"tool_use": {"name": "Write", "input": {"file_path": str(path), "content": content}}}


def _read(path: Path) -> dict[str, Any]:
    return {"tool_use": {"name": "Read", "input": {"file_path": str(path)}}}


def _structured(value: dict[str, Any]) -> dict[str, Any]:
    return {"tool_use": {"name": "StructuredOutput", "input": value}}


def _workflow(args: dict[str, Any]) -> dict[str, Any]:
    return {"tool_use": {"name": "Workflow", "input": {"name": "grind-run", "args": args}}}


def _after(expect: dict[str, Any], step: dict[str, Any]) -> dict[str, Any]:
    """`step`, first checking the previous step's tool results."""
    return {**step, "expect": expect}


def _saw(text: str) -> dict[str, Any]:
    return {"is_error": False, "content_contains": text}


def _role(
    name: str, role: str, prompt: str | list[str], steps: list[dict[str, Any]]
) -> dict[str, Any]:
    return {"name": name, "match": MARK[role], "match_prompt": prompt, "steps": steps}


def _q(header: str, question: str, options: list[str]) -> dict[str, Any]:
    return {
        "question": question,
        "header": header,
        "multiSelect": False,
        "options": [{"label": o, "description": o} for o in options],
    }


def _ask(*questions: dict[str, Any]) -> dict[str, Any]:
    assert len(questions) <= 4, "AskUserQuestion takes at most 4 questions"
    return {"tool_use": {"name": "AskUserQuestion", "input": {"questions": list(questions)}}}


def _dirty_q(carry: bool) -> dict[str, Any]:
    options = DIRTY_OPTS if carry else [o for o in DIRTY_OPTS if not o.startswith("Carry")]
    return _q(
        "Dirty repo",
        "Your checkout has changes: README.md (modified), notes.txt (untracked). "
        "What should grind do with them?",
        options,
    )


MODE_Q = _q("Mode", "How should grind run the goals?", ["Parallel", "Sequential", "Cron"])
MODELS_Q = _q(
    "Models",
    "Which models should the grind roles use?",
    ["Session model for every role (Recommended)", "Stronger planner and reviewer"],
)
MERGE_Q = _q("Merge", "What happens to the feature PR when every goal has landed?", MERGE_OPTS)
PROBLEMS_Q = _q("Problems", "Where do problems found during the run go?", PROBLEM_OPTS)
CI_Q = _q("Local CI", "Run the ci.yml job locally under act before each push?", ["Yes", "No"])
SCRIPTS_Q = _q(
    "Scripts", "Found ./lint and ./test. Run them before each push?", ["Both", "Neither"]
)
REGROUP_Q = _q(
    "Regroup",
    "Regroup #1 into docs (#2-#5) and cli (#6-#9), and pick this run's feature?",
    ["Regroup; run docs first (Recommended)", "Regroup; run cli first", "Keep as is"],
)


# ---- world -----------------------------------------------------------------------


def _seed(h: Harness, meta: str, children: list[int]) -> None:
    """Meta `meta` with `children` as native sub-issues."""
    issues = {int(meta): _issue("meta: backlog", "Tracked as sub-issues.")}
    for n in children:
        issues[n] = _issue(f"child {n}", f"do {n}", parent=int(meta))
    h.write_gh_state(_world(issues))


def _grind_dir(h: Harness) -> Path:
    path = Path(h.repo) / ".clud" / "grind"
    path.mkdir(parents=True, exist_ok=True)
    return path


def _run_json(h: Harness) -> Path:
    path = h.run_facts_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def _dirty(h: Harness) -> None:
    """One modified tracked file and one untracked file."""
    readme = Path(h.repo) / "README.md"
    readme.write_text(readme.read_text(encoding="utf-8") + "my local edit\n", encoding="utf-8")
    (Path(h.repo) / "notes.txt").write_text("scratch\n", encoding="utf-8")


def _goals(children: list[int]) -> list[dict[str, str]]:
    return [{"id": str(n), "title": f"child {n}", "brief": f"do {n}"} for n in children]


def _classify(bugs: list[int], groups: dict[str, list[int]]) -> dict[str, Any]:
    children: list[dict[str, Any]] = [
        {"id": str(n), "track": "bug", "depends_on_bugs": []} for n in bugs
    ]
    for group, members in groups.items():
        children += [
            {"id": str(n), "track": "feature", "group": group, "depends_on_bugs": []}
            for n in members
        ]
    return {
        "children": children,
        "groups": [
            {"name": g, "independent": True, "children": [str(n) for n in ms]}
            for g, ms in groups.items()
        ],
        "order": [c["id"] for c in children],
        "confident": True,
    }


def _classifier(h: Harness, classification: dict[str, Any]) -> dict[str, Any]:
    """The plan-only planner: one read-only look, then its classification."""
    steps = [
        _bash(f"git -C {h.repo} log --oneline -1"),
        _after(OK, _structured(classification)),
    ]
    return _role("classify", "planner", "PLAN-ONLY", steps)


def _prework(meta: str, run_id: str) -> dict[str, Any]:
    url = f"https://github.com/o/r/issues/{meta}#issuecomment-1000"
    body = f"<!-- grind:v1 plan run={run_id} -->"
    steps = [
        _bash(f"gh issue comment {meta} --repo o/r --body '{body}'"),
        _after(OK, _structured({"posted": True, "plan_url": url, "part_urls": [url]})),
    ]
    return _role("prework", "prework", "/grind-prework", steps)


def _goal_roles(h: Harness, goal: str) -> list[dict[str, Any]]:
    """Planner and worker run; the reviewer rejects, so nothing is pushed."""
    at = f"Goal {goal}:"
    plan = {
        "checkout": str(h.repo),
        "branch": f"grind/{goal}",
        "depends_on": [],
        "verify": "true",
        "tasks": [{"id": "t1", "files": [f"{goal}.txt"], "instructions": f"write {goal}.txt"}],
    }
    work = [
        _read(Path(h.repo) / "README.md"),
        _after(OK, _structured({"files_touched": [], "summary": "read only"})),
    ]
    return [
        _role(f"planner:{goal}", "planner", at, [_structured(plan)]),
        _role(f"worker:{goal}", "worker", at, work),
        _role(
            f"reviewer:{goal}",
            "reviewer",
            at,
            [_structured({"approved": False, "summary": "stop here"})],
        ),
    ]


def _dying_planners() -> dict[str, Any]:
    """Every goal planner of the real run fails, so no goal gets further."""
    return _role("planner:goal", "planner", "Base branch:", [{"error": {"status": 400}}])


def _route_and_plan(h: Harness, meta: str, children: list[int]) -> list[dict[str, Any]]:
    """Routing tool, the plan-phase run facts, the plan-only pass, then preflight."""
    repo = h.repo
    plan_only = {
        "repo": str(repo),
        "github_repo": "o/r",
        "main": "main",
        "meta": meta,
        "planOnly": True,
        "goals": _goals(children),
    }
    preflight = " && ".join(
        [
            f"git -C {repo} status --porcelain -uall -- . {EXCLUDE}",
            f"git -C {repo} rev-parse --abbrev-ref HEAD",
            f"git -C {repo} stash list",
            f"git -C {repo} fetch -q origin",
            f"git -C {repo} rev-list --left-right --count origin/main...HEAD",
        ]
    )
    return [
        _bash(f"{TOOL} {meta} --repo o/r"),
        _after(_saw('"meta": true'), _write(_run_json(h), {"phase": "plan"})),
        _after(OK, _workflow(plan_only)),
        _after(OK, _WAIT),
        _bash(preflight),
    ]


def _plan(meta: str, run_id: str, stages: list[dict[str, Any]], **extra: Any) -> dict[str, Any]:
    return {
        "schema": "grind-plan/v1",
        "run_id": run_id,
        "meta": int(meta),
        "repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "preflight": "handled",
        "structure": "simple",
        "stages": stages,
        "deferred_groups": [],
        "feature_merge": None,
        "problem_reporting": "comment",
        "models": {},
        "ci": False,
        "scripts": None,
        "rules": {"stuck_bug": "block_dependents_only", "no_overlap": "bugs_only"},
        **extra,
    }


def _real_run(h: Harness, meta: str, goals: list[int], plan: dict[str, Any]) -> dict[str, Any]:
    args = {
        "repo": str(h.repo),
        "github_repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "meta": meta,
        "ci": False,
        "goals": _goals(goals),
        "plan": plan,
    }
    return _workflow(args)


def _script(roles: list[dict[str, Any]], main: list[dict[str, Any]]) -> dict[str, Any]:
    return {"default_text": "OK", "roles": [*roles, {"name": "main", "steps": main}]}


def _go(
    h: Harness, script: dict[str, Any], answers: dict[str, str], timeout: float = 400
) -> RunResult:
    result = h.run("/grind", script, timeout=timeout, answers=answers)
    assert result.returncode == 0, result.stdout[-3000:]
    notes = [(r["role"], r["note"]) for r in result.requests if r.get("note")]
    assert not notes, notes
    return result


# ---- assertions --------------------------------------------------------------------


def _asks(result: RunResult) -> list[dict[str, Any]]:
    return [
        h
        for h in result.hooks
        if h.get("event") == "PreToolUse" and h.get("tool_name") == "AskUserQuestion"
    ]


def _questions(result: RunResult) -> list[dict[str, Any]]:
    return [q for h in _asks(result) for q in (h.get("tool_input") or {}).get("questions", [])]


def _question(result: RunResult, header: str) -> dict[str, Any]:
    found = [q for q in _questions(result) if q.get("header") == header]
    assert len(found) == 1, (header, _questions(result))
    return found[0]


def _labels(question: dict[str, Any]) -> list[str]:
    return [o["label"] for o in question["options"]]


def _index(result: RunResult, pred: Any, what: str) -> int:
    for i, record in enumerate(result.hooks):
        if record.get("event") == "PreToolUse" and pred(record):
            return i
    raise AssertionError(f"no hook record for {what}: {result.hooks}")


def _by_agent(agent: str | None) -> Any:
    return lambda r: r.get("agent_type") == agent


def _main_bash(fragment: str) -> Any:
    return lambda r: (
        r.get("agent_type") is None
        and r.get("tool_name") == "Bash"
        and fragment in str((r.get("tool_input") or {}).get("command", ""))
    )


def _none_after_prework(result: RunResult, calls: int) -> None:
    """`calls` AskUserQuestion calls before prework and none from anyone after (U10)."""
    _index(result, _by_agent("grind-prework"), "prework")
    assert len(result.questions_before("grind-prework")) == calls, _asks(result)
    assert result.questions_after("grind-prework") == []


def _never_offered_to_grind_roles(result: RunResult) -> None:
    """First layer of U11: no grind agent is even offered AskUserQuestion."""
    grind = [r for r in result.requests if r["role"] != "main"]
    assert grind, [r["role"] for r in result.requests]
    for request in grind:
        assert "AskUserQuestion" not in request.get("tools", []), request["role"]
    asked = [a for a in _asks(result) if str(a.get("agent_type") or "").startswith("grind-")]
    assert asked == [], asked


def _first(result: RunResult) -> dict[str, int]:
    seen: dict[str, int] = {}
    for r in result.requests:
        seen[r["role"]] = min(seen.get(r["role"], r["n"]), r["n"])
    return seen


def _user_changes(h: Harness) -> str:
    return h.git("status", "--porcelain", "-uall", "--", ".", ":(exclude).clud")


def _refs(h: Harness, where: Path | None = None) -> str:
    return h.git("for-each-ref", "--format=%(refname)", cwd=where)


# ---- tests -------------------------------------------------------------------------


def test_u1_u13_u14_order_is_route_plan_preflight_ask_prework_work(harness: Harness) -> None:
    h = harness
    meta, run_id, children = "100", "u1", [101]
    _seed(h, meta, children)
    run = {
        "mode": "sequential",
        "ci": False,
        "scripts": None,
        "preflight": {"action": "none", "branch": "main"},
        "feature_merge": None,
        "problem_reporting": "comment",
        "tracks": {"101": "bug"},
        "meta": int(meta),
        "undo": [],
        "waiting_on_pr": None,
    }
    plan = _plan(meta, run_id, [{"stage": "bugs", "base": "main", "children": children}])
    main = [
        *_route_and_plan(h, meta, children),
        # Clean tree: no dirty-repo question; bugs only: no merge question.
        _after(_saw("main"), _ask(MODE_Q, MODELS_Q)),
        _after(_saw("Sequential"), _ask(PROBLEMS_Q)),
        _after(_saw("Comment"), _read(_run_json(h))),
        _after(OK, _write(_run_json(h), run)),
        _after(OK, _real_run(h, meta, children, plan)),
        _after(OK, _WAIT),
        {"text": "GRIND_DONE"},
    ]
    roles = [
        _classifier(h, _classify(children, {})),
        _prework(meta, run_id),
        *_goal_roles(h, "101"),
    ]
    answers = {"Mode": "Sequential", "Problems": "Comment"}
    result = _go(h, _script(roles, main), answers)

    # U1: routing tool -> plan-only planner -> preflight -> the question round
    # -> prework -> workers.
    tool = _index(result, _main_bash("is_meta_issue.py"), "routing tool")
    planner = _index(result, _by_agent("grind-planner"), "plan-only planner")
    preflight = _index(result, _main_bash("status --porcelain"), "preflight")
    ask = _index(result, lambda r: r.get("tool_name") == "AskUserQuestion", "questions")
    prework = _index(result, _by_agent("grind-prework"), "prework")
    worker = _index(result, _by_agent("grind-editor"), "worker")
    assert tool < planner < preflight < ask < prework < worker, (
        tool,
        planner,
        preflight,
        ask,
        prework,
        worker,
    )
    first = _first(result)
    assert first["classify"] < first["prework"] < first["planner:101"] < first["worker:101"]
    assert "PLAN-ONLY" in json.dumps(result.first_request("classify").get("messages"))
    _none_after_prework(result, 2)
    _never_offered_to_grind_roles(result)

    # U13: a bugs-only plan asks no feature-merge-policy (or dirty-repo) question.
    headers = [q["header"] for q in _questions(result)]
    assert headers == ["Mode", "Models", "Problems"], headers

    # U14: the run facts record every answer.
    recorded = json.loads(_run_json(h).read_text(encoding="utf-8"))
    assert recorded["mode"] == "sequential"
    assert recorded["preflight"] == {"action": "none", "branch": "main"}
    assert recorded["feature_merge"] is None
    assert recorded["problem_reporting"] == "comment"
    assert recorded["tracks"] == {"101": "bug"}
    assert "phase" not in recorded


def test_u3_u8_dirty_feature_plan_offers_carry_and_abort_creates_nothing(
    harness: Harness,
) -> None:
    h = harness
    meta, children = "100", [101, 102]
    _seed(h, meta, children)
    _dirty(h)
    before_issues = h.read_gh_state()["issues"]
    before = (h.git("worktree", "list"), _refs(h), _refs(h, h.origin), h.git("stash", "list"))
    status = _user_changes(h)
    main = [
        *_route_and_plan(h, meta, children),
        _after(_saw("README.md"), _ask(_dirty_q(carry=True), MODE_Q, MODELS_Q)),
        # Abort: remove the plan-phase run facts and create nothing else.
        _after(_saw("Abort"), _bash("clud grind-facts clear")),
        _after(OK, {"text": "Aborted; nothing was created."}),
    ]
    roles = [_classifier(h, _classify([101], {"verbose": [102]}))]
    result = _go(h, _script(roles, main), {"Dirty repo": "Abort"})

    # U3: the dirty-repo question lists the files and offers all four actions,
    # carry included, because the plan has a feature stage.
    dirty = _question(result, "Dirty repo")
    assert "README.md" in dirty["question"], dirty
    assert "notes.txt" in dirty["question"], dirty
    assert _labels(dirty) == DIRTY_OPTS
    assert len(_asks(result)) == 1

    # U8: nothing on GitHub, nothing in git, no run facts, no later role.
    assert h.read_gh_state()["issues"] == before_issues
    writes = [
        c
        for c in h.read_gh_state().get("calls", [])
        if (c[:1] == ["issue"] and c[1:2] != ["view"]) or (c[:1] == ["api"] and "-X" in c)
    ]
    assert writes == [], writes
    assert (h.git("worktree", "list"), _refs(h), _refs(h, h.origin), h.git("stash", "list")) == (
        before
    )
    assert _user_changes(h) == status
    assert not _run_json(h).exists()
    assert not (_grind_dir(h) / "plan.json").exists()
    assert {r["role"] for r in result.requests} == {"main", "classify"}


def test_u4_u5_u13_stash_lives_through_the_run_and_finish_restores_it(
    harness: Harness,
) -> None:
    h = harness
    repo = h.repo
    meta, run_id, children = "100", "u5", [101]
    stash = f"grind-{run_id}"
    _seed(h, meta, children)
    _dirty(h)
    run = {
        "mode": "sequential",
        "ci": False,
        "preflight": {"action": "stash", "branch": "main", "stash": stash},
        "feature_merge": None,
        "problem_reporting": "issue",
        "tracks": {"101": "bug"},
        "meta": int(meta),
    }
    plan = _plan(meta, run_id, [{"stage": "bugs", "base": "main", "children": children}])
    main = [
        *_route_and_plan(h, meta, children),
        _after(_saw("README.md"), _ask(_dirty_q(carry=False), MODE_Q, MODELS_Q)),
        _after(_saw("Stash it"), _ask(PROBLEMS_Q)),
        _after(
            _saw("New issue per problem"),
            _bash(
                f"git -C {repo} stash push -q -u -m {stash} -- . {EXCLUDE} && "
                f"git -C {repo} stash list"
            ),
        ),
        _after(_saw(stash), _read(_run_json(h))),
        _after(OK, _write(_run_json(h), run)),
        _after(OK, _real_run(h, meta, children, plan)),
        _after(OK, _WAIT),
        # The workflow has finished; the stash is still there before Finish.
        _bash(f"git -C {repo} stash list && git -C {repo} status --porcelain -uall -- . {EXCLUDE}"),
        _after(
            _saw(stash),
            _bash(f"git -C {repo} switch -q main && git -C {repo} stash pop -q 'stash@{{0}}'"),
        ),
        _after(OK, {"text": "GRIND_DONE: stash restored"}),
    ]
    roles = [
        _classifier(h, _classify(children, {})),
        _prework(meta, run_id),
        *_goal_roles(h, "101"),
    ]
    answers = {"Dirty repo": "Stash it", "Mode": "Sequential", "Problems": PROBLEM_OPTS[0]}
    result = _go(h, _script(roles, main), answers)

    # U4: bugs-only plan, so no "carry" option. U13: no merge-policy question.
    assert _labels(_question(result, "Dirty repo")) == [
        "Stash it",
        "Commit to a WIP branch",
        "Abort",
    ]
    assert "Merge" not in [q["header"] for q in _questions(result)]
    _none_after_prework(result, 2)

    # U5: the named stash existed while the run ran (the expects above), and
    # Finish popped it and left the user on the starting branch.
    assert h.git("stash", "list") == ""
    assert h.git("branch", "--show-current") == "main"
    readme = (Path(repo) / "README.md").read_text(encoding="utf-8")
    assert "my local edit" in readme
    assert (Path(repo) / "notes.txt").read_text(encoding="utf-8") == "scratch\n"

    # U14: the preflight answer is what the run facts recorded.
    recorded = json.loads(_run_json(h).read_text(encoding="utf-8"))
    assert recorded["preflight"] == {"action": "stash", "branch": "main", "stash": stash}
    assert recorded["problem_reporting"] == "issue"


def test_u6_wip_branch_stays_local_and_finish_returns_to_the_start_branch(
    harness: Harness,
) -> None:
    h = harness
    repo = h.repo
    meta, run_id, children = "100", "u6", [101]
    wip = f"wip/grind-{run_id}"
    _seed(h, meta, children)
    _dirty(h)
    main = [
        *_route_and_plan(h, meta, children),
        _after(_saw("README.md"), _ask(_dirty_q(carry=False), MODE_Q, MODELS_Q)),
        _after(_saw("Commit to a WIP branch"), _ask(PROBLEMS_Q)),
        _after(
            _saw("Comment"),
            _bash(
                " && ".join(
                    [
                        f"git -C {repo} switch -q -c {wip}",
                        f"git -C {repo} add -A -- . {EXCLUDE}",
                        f"git -C {repo} commit -q -m 'grind: WIP before run {run_id}'",
                        f"git -C {repo} switch -q main",
                    ]
                )
            ),
        ),
        # Finish: back on the starting branch; the WIP branch is only reported.
        _after(
            OK,
            _bash(
                f"git -C {repo} fetch -q origin && git -C {repo} switch -q main && "
                f"git -C {repo} branch --list 'wip/*'"
            ),
        ),
        _after(_saw(wip), {"text": f"GRIND_DONE: your changes are on {wip}"}),
    ]
    roles = [_classifier(h, _classify(children, {}))]
    answers = {"Dirty repo": "Commit to a WIP branch", "Mode": "Sequential", "Problems": "Comment"}
    _go(h, _script(roles, main), answers)

    assert h.git("branch", "--show-current") == "main"
    assert _user_changes(h) == ""
    files = h.git("show", "--name-only", "--format=", wip).split()
    assert sorted(files) == ["README.md", "notes.txt"], files
    assert f"refs/heads/{wip}" in _refs(h)
    assert "wip/" not in _refs(h, h.origin)


def test_u7_carry_moves_the_changes_into_the_feature_worktree(harness: Harness) -> None:
    h = harness
    repo = h.repo
    meta, run_id, children = "100", "u7", [101, 102]
    feature = f"grind/meta-{meta}-{run_id}"
    wt = _grind_dir(h) / "worktrees" / "feature"
    _seed(h, meta, children)
    _dirty(h)
    carry = f"grind-{run_id}-carry"
    main = [
        *_route_and_plan(h, meta, children),
        _after(_saw("README.md"), _ask(_dirty_q(carry=True), MODE_Q, MODELS_Q)),
        _after(_saw("Carry into the grind worktree"), _ask(MERGE_Q, PROBLEMS_Q)),
        _after(
            _saw("Decide later"),
            _bash(
                f"git -C {repo} stash push -q -u -m {carry} -- . {EXCLUDE} && "
                f"git -C {repo} stash list"
            ),
        ),
        # Section 4b: the one feature worktree gets the carried changes as the
        # feature branch's first commit.
        _after(
            _saw(carry),
            _bash(
                " && ".join(
                    [
                        f"git -C {repo} worktree add -q {wt} -b {feature} origin/main",
                        f"git -C {wt} stash pop -q 'stash@{{0}}'",
                        f"git -C {wt} add -A",
                        f"git -C {wt} commit -q -m 'grind: carry uncommitted changes from main'",
                        f"git -C {wt} push -q -u origin {feature}",
                    ]
                )
            ),
        ),
        _after(OK, {"text": "CARRIED"}),
    ]
    roles = [_classifier(h, _classify([101], {"verbose": [102]}))]
    answers = {
        "Dirty repo": "Carry into the grind worktree",
        "Mode": "Sequential",
        "Merge": "Decide later",
        "Problems": "Comment",
    }
    result = _go(h, _script(roles, main), answers)

    assert _labels(_question(result, "Dirty repo")) == DIRTY_OPTS
    # The changes are in the feature worktree and on the pushed feature branch.
    assert "my local edit" in (wt / "README.md").read_text(encoding="utf-8")
    assert (wt / "notes.txt").read_text(encoding="utf-8") == "scratch\n"
    shown = h.git("show", "--name-only", "--format=%s", f"refs/heads/{feature}", cwd=h.origin)
    subject, *files = [line for line in shown.splitlines() if line.strip()]
    assert subject == "grind: carry uncommitted changes from main", shown
    assert sorted(files) == ["README.md", "notes.txt"], shown
    # The user's checkout is clean, still on main, and holds no stash.
    assert _user_changes(h) == ""
    assert "my local edit" not in (Path(repo) / "README.md").read_text(encoding="utf-8")
    assert h.git("branch", "--show-current") == "main"
    assert h.git("stash", "list") == ""


def test_u9_finish_returns_to_a_non_main_start_branch(harness: Harness) -> None:
    h = harness
    repo = h.repo
    meta, children = "100", [101]
    _seed(h, meta, children)
    # Start on `topic`, one commit behind origin/main.
    h.git("switch", "-q", "-c", "topic")
    h.git("switch", "-q", "main")
    h.git("commit", "-q", "--allow-empty", "-m", "main moved")
    h.git("push", "-q", "origin", "main")
    h.git("switch", "-q", "topic")
    topic = h.git("rev-parse", "topic")
    run = {
        "mode": "sequential",
        "ci": False,
        "preflight": {"action": "none", "branch": "topic"},
        "problem_reporting": "comment",
        "tracks": {"101": "bug"},
        "meta": int(meta),
    }
    main = [
        *_route_and_plan(h, meta, children),
        # Preflight saw the branch and that it is behind origin/main.
        _after(_saw("topic"), _ask(MODE_Q, MODELS_Q)),
        _after(_saw("Sequential"), _ask(PROBLEMS_Q)),
        _after(_saw("Comment"), _read(_run_json(h))),
        _after(OK, _write(_run_json(h), run)),
        # Sequential mode works from origin/main in the local checkout.
        _after(OK, _bash(f"git -C {repo} switch -q main")),
        # Finish: back to the recorded branch, never fast-forwarded.
        _after(
            OK,
            _bash(
                f"git -C {repo} fetch -q origin && git -C {repo} switch -q topic && "
                f"git -C {repo} rev-parse --abbrev-ref HEAD"
            ),
        ),
        _after(_saw("topic"), {"text": "GRIND_DONE: back on topic"}),
    ]
    roles = [_classifier(h, _classify(children, {}))]
    answers = {"Mode": "Sequential", "Problems": "Comment"}
    result = _go(h, _script(roles, main), answers)

    preflight = [
        r
        for r in result.requests
        if r["role"] == "main" and "1\\t0" in json.dumps(r.get("messages"))
    ]
    assert preflight, "preflight never reported the branch as 1 behind origin/main"
    assert h.git("branch", "--show-current") == "topic"
    assert h.git("rev-parse", "topic") == topic
    recorded = json.loads(_run_json(h).read_text(encoding="utf-8"))
    assert recorded["preflight"] == {"action": "none", "branch": "topic"}


def test_u10_u12_every_question_in_one_round_and_none_after_prework(
    harness: Harness,
) -> None:
    h = harness
    repo = h.repo
    meta, run_id = "1", "u10"
    docs, cli = [2, 3, 4, 5], [6, 7, 8, 9]
    h.write_gh_state(regroupable())
    _dirty(h)
    stash = f"grind-{run_id}"
    run = {
        "mode": "sequential",
        "ci": False,
        "scripts": None,
        "preflight": {"action": "stash", "branch": "main", "stash": stash},
        "feature_merge": "later",
        "problem_reporting": "issue",
        "tracks": {str(n): "feature" for n in docs + cli},
        "meta": int(meta),
    }
    branch = f"grind/meta-{meta}-{run_id}"
    stage = {
        "stage": "feature",
        "group": "docs",
        "sub_meta": None,
        "branch": branch,
        "base": branch,
        "children": docs,
        "depends_on_bugs": {},
    }
    plan = _plan(
        meta,
        run_id,
        [stage],
        deferred_groups=[{"group": "cli", "sub_meta": None, "children": cli}],
        feature_merge="later",
        problem_reporting="issue",
    )
    main = [
        *_route_and_plan(h, meta, docs + cli),
        # Call 1: the repo and the plan. Call 2: the run's policies.
        _after(_saw("README.md"), _ask(_dirty_q(carry=True), REGROUP_Q, MODE_Q, MODELS_Q)),
        _after(_saw("Keep as is"), _ask(CI_Q, SCRIPTS_Q, MERGE_Q, PROBLEMS_Q)),
        _after(
            _saw("Decide later"),
            _bash(f"git -C {repo} stash push -q -u -m {stash} -- . {EXCLUDE}"),
        ),
        _after(OK, _read(_run_json(h))),
        _after(OK, _write(_run_json(h), run)),
        _after(OK, _real_run(h, meta, docs, plan)),
        _after(OK, _WAIT),
        {"text": "GRIND_DONE"},
    ]
    roles = [
        _classifier(h, _classify([], {"docs": docs, "cli": cli})),
        _prework(meta, run_id),
        _dying_planners(),
    ]
    answers = {
        "Dirty repo": "Stash it",
        "Regroup": "Keep as is",
        "Mode": "Sequential",
        "Local CI": "No",
        "Scripts": "Neither",
        "Merge": "Decide later",
        "Problems": PROBLEM_OPTS[0],
    }
    result = _go(h, _script(roles, main), answers)

    # U12: every item, in at most 2 calls of at most 4 questions, none twice.
    calls = result.questions_before("grind-prework")
    assert len(calls) == 2, calls
    assert all(len(c["tool_input"]["questions"]) <= 4 for c in calls), calls
    headers = [q["header"] for q in _questions(result)]
    assert sorted(headers) == sorted(
        ["Dirty repo", "Regroup", "Mode", "Models", "Local CI", "Scripts", "Merge", "Problems"]
    ), headers
    texts = [q["question"] for q in _questions(result)]
    assert len(set(texts)) == len(texts), texts

    # U10: with every question-triggering condition present, every question
    # precedes prework and nobody (main session included) asks afterwards.
    _none_after_prework(result, 2)
    _never_offered_to_grind_roles(result)
    plan_only = _index(result, _by_agent("grind-planner"), "plan-only planner")
    ask = _index(result, lambda r: r.get("tool_name") == "AskUserQuestion", "questions")
    assert plan_only < ask

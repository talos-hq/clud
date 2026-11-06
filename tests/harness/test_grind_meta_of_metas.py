"""Meta of metas on the real Claude Code (#1412, M1-M9 of #1392).

The regroup (router skill section 2b) is main-session behaviour, so the
scripted main session runs the exact `gh` commands the skill names:

- reuse a user-made sub-meta in place with `gh issue edit <n> --title ... --body ...`
  (the new body starts with `<!-- grind:v1 -->` and keeps the old body in
  `<details><summary>Previous content</summary>`),
- create a surplus sub-meta with `gh issue create ... --label grind:meta` and
  attach it with `gh api -X POST repos/o/r/issues/<T>/sub_issues -F sub_issue_id=<id>`,
- move each feature child with
  `gh api -X POST repos/o/r/issues/<sub>/sub_issues -F sub_issue_id=<id> -F replace_parent=true`.

Bodies go inline with `--body`, which keeps the scripted commands short.
Every change is written to the run facts' `undo` before its command runs, and
the plan fields to `.clud/grind/plan.json`. The tests then check the fake
GitHub state against hard-coded expectations, plus the undo record against
the before/after states. A second regroup over a regrouped world changes
nothing (M5, later run). M9 has GitHub refuse a move (8 levels, 100
sub-issues) and the router stop before prework. M6 runs `grind-run` itself:
one feature stage runs, the deferred group's goals come back with status
`deferred`.

Script note: a step's `expect` checks the tool results of the *previous*
step, so an expectation about a command sits on the step after it.
"""

from __future__ import annotations

import json
import re
import shlex
from pathlib import Path
from typing import Any

from tests.harness.harness import Harness, RunResult
from tests.harness.worlds import (
    GRIND_MARKER,
    _issue,
    _world,
    regroupable,
    with_grind_sub_meta,
)

MARK = {
    "prework": "You are the /grind prework role",
    "planner": "You are the /grind planner",
    "worker": "You are the /grind editor",
    "reviewer": "You are the /grind reviewer",
    "integrator": "You are the /grind integrator",
    "lander": "You are the /grind lander",
}
TOP = 1
V1 = "<!-- grind:v1 -->"
PLAN_MARKER = "<!-- grind:v1 plan run="
RUN_ID = "r-1412"
OK = {"is_error": False}
PREVIOUS = "<details><summary>Previous content</summary>"
EMPTY = "no children after regrouping"


def _structured(value: dict[str, Any]) -> dict[str, Any]:
    return {"tool_use": {"name": "StructuredOutput", "input": value}}


def _bash(command: str) -> dict[str, Any]:
    return {"tool_use": {"name": "Bash", "input": {"command": command, "description": "grind"}}}


def _write(path: Path, content: str) -> dict[str, Any]:
    return {"tool_use": {"name": "Write", "input": {"file_path": str(path), "content": content}}}


def _after(expect: dict[str, Any], step: dict[str, Any]) -> dict[str, Any]:
    """`step`, first checking the previous step's tool results."""
    return {**step, "expect": expect}


def _role(
    name: str, role: str, prompt: str | list[str], steps: list[dict[str, Any]]
) -> dict[str, Any]:
    return {"name": name, "match": MARK[role], "match_prompt": prompt, "steps": steps}


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


def _told(result: RunResult) -> str:
    seen = [r["messages"] for r in result.requests if r["role"] == "main"]
    return json.dumps(seen, ensure_ascii=False) + result.stdout


def _run_path(h: Harness) -> Path:
    return h.run_facts_path()


def _plan_path(h: Harness) -> Path:
    return Path(h.repo) / ".clud" / "grind" / "plan.json"


def _chain(steps: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Each step after the first expects the previous one to have succeeded."""
    return [s if i == 0 else _after(OK, s) for i, s in enumerate(steps)]


def _main_only(steps: list[dict[str, Any]]) -> dict[str, Any]:
    tail = _after(OK, {"text": "GRIND_DONE"}) if steps else {"text": "GRIND_DONE"}
    return {"default_text": "OK", "roles": [{"name": "main", "steps": [*_chain(steps), tail]}]}


# ---- fake GitHub views ---------------------------------------------------------


def _iss(state: dict[str, Any], n: int) -> dict[str, Any]:
    return state["issues"][str(n)]


def _parent(state: dict[str, Any], n: int) -> int | None:
    return _iss(state, n).get("parent")


def _subs(state: dict[str, Any], n: int) -> set[int]:
    return {int(s["number"]) for s in _iss(state, n).get("sub_issues", [])}


def _calls(state: dict[str, Any], *head: str) -> list[list[str]]:
    return [c for c in state.get("calls", []) if c[: len(head)] == list(head)]


def _marked(issue: dict[str, Any]) -> bool:
    body = issue.get("body", "")
    return V1 in body or GRIND_MARKER in body or "grind:meta" in issue.get("labels", [])


# ---- the router's section 2b, as gh commands -----------------------------------


def _group_body(name: str, kids: list[int]) -> str:
    return f"{V1}\nFeature group: {name}\n\n" + "\n".join(f"- #{k}" for k in kids)


def _previous(old: str) -> str:
    return f"\n\n{PREVIOUS}\n\n{old}\n\n</details>"


def _edit(n: int, title: str, body: str) -> str:
    return f"gh issue edit {n} --repo o/r --title {shlex.quote(title)} --body {shlex.quote(body)}"


def _create(title: str, body: str) -> str:
    return (
        f"gh issue create --repo o/r --title {shlex.quote(title)} "
        f"--body {shlex.quote(body)} --label grind:meta"
    )


def _attach(parent: int, child: int, *, replace: bool) -> str:
    cmd = f"gh api -X POST repos/o/r/issues/{parent}/sub_issues -F sub_issue_id={child}"
    return cmd + (" -F replace_parent=true" if replace else "")


def _rewrite(n: int, old: dict[str, Any]) -> dict[str, Any]:
    return {"op": "rewrite", "issue": n, "title": old["title"], "body": old["body"]}


def _regroup(
    state: dict[str, Any], groups: list[tuple[str, list[int]]]
) -> tuple[list[str], list[dict[str, Any]], dict[str, int]]:
    """Section 2b against `state`: (commands, undo entries, group -> sub-meta)."""
    issues = state["issues"]
    subs = [n for n in sorted(_subs(state, TOP)) if _subs(state, n)]
    user = [n for n in subs if not _marked(_iss(state, n))]
    grind = [n for n in subs if _marked(_iss(state, n))]
    protected = {c for g in grind for c in _subs(state, g)}
    # fake_gh numbers a new issue after every issue *and* PR number.
    next_id = max([int(k) for k in issues] + [int(p["number"]) for p in state.get("prs", [])]) + 1
    cmds: list[str] = []
    undo: list[dict[str, Any]] = []
    where: dict[str, int] = {}
    if len(groups) == 1:
        where[groups[0][0]] = TOP
    else:
        for name, kids in groups:
            if kids and all(k in protected for k in kids):
                where[name] = _parent(state, kids[0])
                continue
            title = f"grind: {name}"
            if user:
                n = user.pop(0)
                old = _iss(state, n)
                undo.append(_rewrite(n, old))
                cmds.append(_edit(n, title, _group_body(name, kids) + _previous(old["body"])))
            else:
                n = next_id
                next_id += 1
                undo.append({"op": "create", "issue": n})
                cmds.append(_create(title, _group_body(name, kids)))
                undo.append({"op": "reparent", "issue": n, "from": None, "to": TOP})
                cmds.append(_attach(TOP, n, replace=False))
            where[name] = n
        for n in user:
            old = _iss(state, n)
            undo.append(_rewrite(n, old))
            cmds.append(_edit(n, old["title"], f"{V1}\n{EMPTY}" + _previous(old["body"])))
    for name, kids in groups:
        for k in kids:
            if k in protected:
                continue
            old_parent = _parent(state, k)
            if old_parent != where[name]:
                undo.append({"op": "reparent", "issue": k, "from": old_parent, "to": where[name]})
                cmds.append(_attach(where[name], k, replace=True))
    return cmds, undo, where


def _plan_doc(
    groups: list[tuple[str, list[int]]], where: dict[str, int], bugs: list[int]
) -> dict[str, Any]:
    feature = [
        {
            "stage": "feature",
            "group": name,
            "sub_meta": None if where[name] == TOP else where[name],
            "children": [str(k) for k in kids],
        }
        for name, kids in groups
    ]
    return {
        "schema": "grind-plan/v1",
        "run_id": RUN_ID,
        "meta": str(TOP),
        "structure": "meta_of_metas",
        "stages": [
            {"stage": "bugs", "base": "main", "children": [str(b) for b in bugs]},
            feature[0],
        ],
        "deferred_groups": [
            {"group": f["group"], "sub_meta": f["sub_meta"], "children": f["children"]}
            for f in feature[1:]
        ],
    }


def _regroup_run(
    h: Harness,
    world: dict[str, Any],
    groups: list[tuple[str, list[int]]],
    bugs: list[int],
) -> tuple[dict[str, Any], dict[str, Any], list[dict[str, Any]], dict[str, Any]]:
    """Seed `world`, run the scripted regroup; (before, after, undo, plan)."""
    h.write_gh_state(world)
    before = h.read_gh_state()
    cmds, undo, where = _regroup(before, groups)
    plan = _plan_doc(groups, where, bugs)
    run = {"mode": "sequential", "meta": str(TOP), "undo": undo, "waiting_on_pr": None}
    steps = [
        _write(_run_path(h), json.dumps(run, indent=1)),
        *[_bash(c) for c in cmds],
        _write(_plan_path(h), json.dumps(plan, indent=1)),
    ]
    result = h.run("start grind", _main_only(steps), timeout=300)
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)
    after = h.read_gh_state()
    saved_undo = json.loads(_run_path(h).read_text(encoding="utf-8"))["undo"]
    saved_plan = json.loads(_plan_path(h).read_text(encoding="utf-8"))
    return before, after, saved_undo, saved_plan


# ---- worlds built here ---------------------------------------------------------


def _backlog_with_bugs() -> dict[str, Any]:
    """`regroupable` (docs 2-5, cli 6-9 under #1) plus bugs #10 and #11."""
    state = regroupable()
    for n in (10, 11):
        state["issues"][str(n)] = _issue(f"bug {n}", f"fix {n}", parent=TOP)
        _iss(state, TOP)["sub_issues"].append({"number": n, "state": "open"})
    return state


def _two_user_metas() -> dict[str, Any]:
    return _world(
        {
            1: _issue("meta: top", "Top-level meta."),
            2: _issue("meta: docs stuff", "User notes on docs.", parent=1),
            3: _issue("meta: misc", "User notes on misc.", parent=1),
            4: _issue("docs a", "do", parent=2),
            5: _issue("docs b", "do", parent=2),
            6: _issue("docs c", "do", parent=1),
            7: _issue("cli a", "do", parent=3),
            8: _issue("cli b", "do", parent=1),
            9: _issue("cli c", "do", parent=1),
            10: _issue("bug", "fix", parent=1),
        }
    )


def _one_user_meta() -> dict[str, Any]:
    return _world(
        {
            1: _issue("meta: top", "Top-level meta."),
            2: _issue("meta: user group", "The user's own grouping.", parent=1),
            4: _issue("docs a", "do", parent=2),
            5: _issue("docs b", "do", parent=2),
            6: _issue("docs c", "do", parent=1),
            7: _issue("cli a", "do", parent=1),
            8: _issue("cli b", "do", parent=1),
            9: _issue("cli c", "do", parent=1),
            10: _issue("bug", "fix", parent=1),
        }
    )


def _three_user_metas() -> dict[str, Any]:
    return _world(
        {
            1: _issue("meta: top", "Top-level meta."),
            2: _issue("meta: first", "First user group.", parent=1),
            3: _issue("meta: second", "Second user group.", parent=1),
            4: _issue("meta: third", "Third user group, soon empty.", parent=1),
            5: _issue("docs a", "do", parent=2),
            6: _issue("docs b", "do", parent=2),
            7: _issue("cli a", "do", parent=3),
            8: _issue("cli b", "do", parent=4),
            9: _issue("docs c", "do", parent=1),
            10: _issue("cli c", "do", parent=1),
            11: _issue("bug", "fix", parent=1),
        }
    )


def _grind_meta_plus_flat() -> dict[str, Any]:
    """`with_grind_sub_meta` (#3 marked, children 4 and 5) plus flat #6-#8."""
    state = with_grind_sub_meta()
    for n in (6, 7, 8):
        state["issues"][str(n)] = _issue(f"cli {n}", "do", parent=TOP)
        _iss(state, TOP)["sub_issues"].append({"number": n, "state": "open"})
    return state


def _too_deep() -> dict[str, Any]:
    """A user sub-meta chain #2 -> #8 under #1, so #8 sits at level 8."""
    issues = {1: _issue("meta: top", "Top-level meta.")}
    for n in range(2, 9):
        issues[n] = _issue(f"meta: level {n}", "nested", parent=n - 1)
    issues[9] = _issue("docs a", "do", parent=1)
    issues[10] = _issue("bug", "fix", parent=1)
    return _world(issues)


# ---- M1-M5, M7, M8: the regroup itself -----------------------------------------


def test_m1_regroup_creates_one_sub_meta_per_group_and_leaves_bugs(harness: Harness) -> None:
    groups = [("docs", [2, 3, 4, 5]), ("cli", [6, 7, 8, 9])]
    _, after, _, plan = _regroup_run(harness, _backlog_with_bugs(), groups, [10, 11])
    docs, cli = 12, 13
    for n, name in ((docs, "docs"), (cli, "cli")):
        issue = _iss(after, n)
        assert issue["title"] == f"grind: {name}", issue
        assert issue["body"].startswith(V1), issue
        assert "grind:meta" in issue["labels"], issue
        assert _parent(after, n) == TOP
    assert all(_parent(after, k) == docs for k in (2, 3, 4, 5))
    assert all(_parent(after, k) == cli for k in (6, 7, 8, 9))
    assert _parent(after, 10) == TOP
    assert _parent(after, 11) == TOP
    assert _subs(after, TOP) == {10, 11, docs, cli}
    assert len(_calls(after, "issue", "create")) == 2
    assert plan["structure"] == "meta_of_metas"
    assert plan["stages"][1]["sub_meta"] == docs
    assert plan["deferred_groups"] == [
        {"group": "cli", "sub_meta": cli, "children": ["6", "7", "8", "9"]}
    ]


def test_m2_user_sub_metas_are_reused_in_place(harness: Harness) -> None:
    groups = [("docs", [4, 5, 6]), ("cli", [7, 8, 9])]
    before, after, _, _ = _regroup_run(harness, _two_user_metas(), groups, [10])
    assert not _calls(after, "issue", "create")
    assert set(after["issues"]) == set(before["issues"])
    for n, name in ((2, "docs"), (3, "cli")):
        issue = _iss(after, n)
        old = _iss(before, n)["body"]
        assert issue["title"] == f"grind: {name}", issue
        assert issue["body"].startswith(V1), issue
        assert f"{PREVIOUS}\n\n{old}\n\n</details>" in issue["body"], issue
    assert _subs(after, 2) == {4, 5, 6}
    assert _subs(after, 3) == {7, 8, 9}
    assert _subs(after, TOP) == {2, 3, 10}


def test_m3_existing_sub_metas_are_reused_first_then_surplus_created(harness: Harness) -> None:
    groups = [("docs", [4, 5, 6]), ("cli", [7, 8, 9])]
    before, after, _, _ = _regroup_run(harness, _one_user_meta(), groups, [10])
    creates = _calls(after, "issue", "create")
    assert len(creates) == 1, creates
    assert _iss(after, 2)["title"] == "grind: docs"
    assert _iss(before, 2)["body"] in _iss(after, 2)["body"]
    assert _iss(after, 11)["title"] == "grind: cli"
    assert _parent(after, 11) == TOP
    assert _subs(after, 2) == {4, 5, 6}
    assert _subs(after, 11) == {7, 8, 9}
    assert _subs(after, TOP) == {2, 10, 11}


def test_m4_leftover_user_sub_meta_says_it_has_no_children(harness: Harness) -> None:
    groups = [("docs", [5, 6, 9]), ("cli", [7, 8, 10])]
    before, after, _, _ = _regroup_run(harness, _three_user_metas(), groups, [11])
    assert not _calls(after, "issue", "create")
    left = _iss(after, 4)
    assert EMPTY in left["body"], left
    assert f"{PREVIOUS}\n\n{_iss(before, 4)['body']}\n\n</details>" in left["body"], left
    assert not _subs(after, 4)
    assert _parent(after, 4) == TOP
    assert _subs(after, 2) == {5, 6, 9}
    assert _subs(after, 3) == {7, 8, 10}


def test_m5_grind_made_sub_meta_and_its_children_are_untouched(harness: Harness) -> None:
    groups = [("group", [4, 5]), ("cli", [6, 7, 8])]
    before, after, undo, _ = _regroup_run(harness, _grind_meta_plus_flat(), groups, [2])
    kept = _iss(after, 3)
    old = _iss(before, 3)
    assert (kept["title"], kept["body"], kept["labels"]) == (
        old["title"],
        old["body"],
        old["labels"],
    )
    assert _parent(after, 4) == 3
    assert _parent(after, 5) == 3
    assert _subs(after, 3) == {4, 5}
    assert not [c for c in _calls(after, "issue", "edit") if c[2] == "3"]
    moved = [c for c in _calls(after, "api") if "sub_issue_id=4" in c or "sub_issue_id=5" in c]
    assert not moved, moved
    assert all(u["issue"] not in (3, 4, 5) for u in undo), undo
    assert _subs(after, 9) == {6, 7, 8}
    assert _parent(after, 2) == TOP


def test_m5_sub_metas_rewritten_by_an_earlier_run_are_kept(harness: Harness) -> None:
    """Scenario 3, run 2: the user's sub-metas rewritten in run 1 now carry
    the marker, so the next run keeps them and changes nothing."""
    groups = [("docs", [4, 5, 6]), ("cli", [7, 8, 9])]
    _, first, _, _ = _regroup_run(harness, _two_user_metas(), groups, [10])
    assert _iss(first, 2)["body"].startswith(V1)
    # Claude Code's Write refuses to overwrite a file this session never read.
    _run_path(harness).unlink()
    _plan_path(harness).unlink()
    # The next `/grind` is a new session, with its own run facts (#1337).
    harness.new_session()
    before, after, undo, plan = _regroup_run(harness, {**first, "calls": []}, groups, [10])
    assert undo == []
    assert not _calls(after, "issue")
    assert not _calls(after, "api")
    assert after["issues"] == before["issues"]
    assert plan["stages"][1]["sub_meta"] == 2
    assert plan["deferred_groups"] == [
        {"group": "cli", "sub_meta": 3, "children": ["7", "8", "9"]}
    ]


def test_m7_one_feature_group_creates_no_sub_meta(harness: Harness) -> None:
    state = _backlog_with_bugs()
    groups = [("docs", [2, 3, 4, 5, 6, 7, 8, 9])]
    before, after, undo, plan = _regroup_run(harness, state, groups, [10, 11])
    assert not _calls(after, "issue", "create")
    assert not _calls(after, "issue", "edit")
    assert not _calls(after, "api")
    assert set(after["issues"]) == set(before["issues"])
    assert all(_parent(after, k) == TOP for k in range(2, 12))
    assert undo == []
    assert plan["stages"][1]["sub_meta"] is None
    assert plan["deferred_groups"] == []


def test_m8_undo_records_every_create_reparent_and_rewrite(harness: Harness) -> None:
    groups = [("docs", [4, 5, 6]), ("cli", [7, 8, 9])]
    before, after, undo, _ = _regroup_run(harness, _one_user_meta(), groups, [10])
    ops = {u["op"] for u in undo}
    assert ops == {"create", "reparent", "rewrite"}, undo
    creates = [u["issue"] for u in undo if u["op"] == "create"]
    new = sorted(set(map(int, after["issues"])) - set(map(int, before["issues"])))
    assert creates == new == [11]
    for u in undo:
        if u["op"] == "rewrite":
            old = _iss(before, u["issue"])
            assert (u["title"], u["body"]) == (old["title"], old["body"]), u
            assert _iss(after, u["issue"])["body"] != old["body"]
        if u["op"] == "reparent":
            assert set(u) == {"op", "issue", "from", "to"}, u
            was = None if u["issue"] in creates else _parent(before, u["issue"])
            assert u["from"] == was, u
            assert _parent(after, u["issue"]) == u["to"], u
    reparented = {u["issue"] for u in undo if u["op"] == "reparent"}
    changed = {
        int(k)
        for k in after["issues"]
        if k not in before["issues"] or _parent(after, int(k)) != _parent(before, int(k))
    }
    assert reparented == changed, (reparented, changed)
    rewritten = {u["issue"] for u in undo if u["op"] == "rewrite"}
    edited = {
        int(k)
        for k in before["issues"]
        if (_iss(after, int(k))["title"], _iss(after, int(k))["body"])
        != (_iss(before, int(k))["title"], _iss(before, int(k))["body"])
    }
    assert rewritten == edited, (rewritten, edited)
    mutating = len(_calls(after, "issue", "create")) + len(_calls(after, "issue", "edit"))
    mutating += len(_calls(after, "api"))
    assert len(undo) == mutating, (undo, after["calls"])


# ---- M9: nesting guard ---------------------------------------------------------


def _too_full() -> dict[str, Any]:
    """A user sub-meta #2 already holding 100 sub-issues (#3-#102), plus a
    loose feature child #103 and a bug #104 under #1."""
    issues = {
        1: _issue("meta: top", "Top-level meta."),
        2: _issue("meta: big group", "The user's big group.", parent=1),
    }
    for n in range(3, 103):
        issues[n] = _issue(f"docs {n}", "do", parent=2)
    issues[103] = _issue("docs extra", "do", parent=1)
    issues[104] = _issue("bug", "fix", parent=1)
    return _world(issues)


def _guarded_move_fails(
    harness: Harness, world: dict[str, Any], parent: int, child: int, reason: str
) -> None:
    """Move `child` under `parent`; GitHub refuses with `reason`, and the
    router reports it and stops before prework."""
    harness.write_gh_state(world)
    move = _attach(parent, child, replace=True)
    undo = [{"op": "reparent", "issue": child, "from": TOP, "to": parent}]
    run = {"mode": "sequential", "meta": str(TOP), "undo": undo, "waiting_on_pr": None}
    steps = [
        _write(_run_path(harness), json.dumps(run, indent=1)),
        _after(OK, _bash(move)),
        _after(
            {"is_error": True, "content_contains": reason},
            {"text": f"regroup failed: {reason}; keeping #1 as is. GRIND_DONE"},
        ),
    ]
    script = {"default_text": "OK", "roles": [{"name": "main", "steps": steps}]}
    result = harness.run("start grind", script, timeout=300)
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)
    after = harness.read_gh_state()
    posts = [c for c in _calls(after, "api") if "replace_parent=true" in c]
    assert len(posts) == 1, after["calls"]
    assert _parent(after, child) == TOP
    assert child not in _subs(after, parent)
    assert _subs(after, parent) == _subs(world, parent)
    assert "regroup failed" in _told(result)
    assert "prework" not in {r["role"] for r in result.requests}
    assert not any(h.get("agent_type") == "grind-prework" for h in result.hooks)
    comments = _iss(after, TOP).get("comments", [])
    assert not [c for c in comments if PLAN_MARKER in c.get("body", "")], comments


def test_m9_too_deep_regroup_fails_before_prework(harness: Harness) -> None:
    _guarded_move_fails(harness, _too_deep(), 8, 9, "8 levels")


def test_m9_full_parent_regroup_fails_before_prework(harness: Harness) -> None:
    _guarded_move_fails(harness, _too_full(), 2, 103, "100 sub-issues")


# ---- M6: feature pick, one feature per run -------------------------------------


def _goal_roles(h: Harness, goal: str) -> list[dict[str, Any]]:
    """One goal's planner, worker, reviewer, integrator and lander.

    The branch is `grind/goal-<id>`, not `grind/<id>`: fake_gh reads a
    target whose last path segment is a number as a PR number.
    """
    branch = f"grind/goal-{goal}"
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
        _role(
            f"lander:{goal}",
            "lander",
            [at, "Fix rounds used: 0 of"],
            [
                _bash(f"gh pr merge {branch} --admin --squash"),
                _after(OK, _structured({"status": "merged", "summary": "green"})),
            ],
        ),
    ]


def _picked_world() -> dict[str, Any]:
    """Top #1 with sub-metas #10 (docs: 2, 3) and #11 (cli: 4, 5)."""
    return _world(
        {
            1: _issue("meta: top", "Top-level meta."),
            2: _issue("docs a", "do", parent=10),
            3: _issue("docs b", "do", parent=10),
            4: _issue("cli a", "do", parent=11),
            5: _issue("cli b", "do", parent=11),
            10: _issue("grind: docs", f"{V1}\ndocs", labels=["grind:meta"], parent=1),
            11: _issue("grind: cli", f"{V1}\ncli", labels=["grind:meta"], parent=1),
        }
    )


def test_m6_only_the_picked_feature_group_is_worked(harness: Harness) -> None:
    harness.write_gh_state(_picked_world())
    run = _run_path(harness)
    run.parent.mkdir(parents=True, exist_ok=True)
    run.write_text(json.dumps({"mode": "sequential", "meta": str(TOP)}), encoding="utf-8")
    plan = {
        "schema": "grind-plan/v1",
        "run_id": RUN_ID,
        "meta": str(TOP),
        "repo": "o/r",
        "main": "main",
        "mode": "sequential",
        "preflight": {"action": "none", "branch": "main"},
        "structure": "meta_of_metas",
        "stages": [
            {"stage": "feature", "group": "docs", "sub_meta": 10, "children": ["2", "3"]},
            {"stage": "feature", "group": "cli", "sub_meta": 11, "children": ["4", "5"]},
        ],
        "deferred_groups": [{"group": "cli", "sub_meta": 11, "children": ["4", "5"]}],
        "feature_merge": "later",
        "problem_reporting": "issue",
        "models": {},
        "ci": False,
        "scripts": {},
        "rules": {},
    }
    url = f"https://github.com/o/r/issues/{TOP}#issuecomment-1000"
    # Marker only: clud's rm-vars hook reads the backticks of a ```json fence
    # inside a single-quoted --body as a command substitution and blocks the
    # whole command. This test is about the feature pick, not the plan body.
    body = f"{PLAN_MARKER}{RUN_ID} -->"
    prework = _role(
        "prework",
        "prework",
        "/grind-prework",
        [
            _bash(f"gh issue comment {TOP} --repo o/r --body {shlex.quote(body)}"),
            _after(OK, _structured({"posted": True, "plan_url": url, "part_urls": [url]})),
        ],
    )
    goals = ["2", "3", "4", "5"]
    args = {
        "repo": str(harness.repo),
        "main": "main",
        "mode": "sequential",
        "meta": str(TOP),
        "plan": plan,
        "goals": [{"id": g, "title": f"child {g}", "brief": f"do {g}"} for g in goals],
    }
    main = {
        "name": "main",
        "steps": [
            {"tool_use": {"name": "Workflow", "input": {"name": "grind-run", "args": args}}},
            {"text": "GRIND_DONE"},
        ],
    }
    roles = [prework, *_goal_roles(harness, "2"), *_goal_roles(harness, "3"), main]
    result = harness.run("start grind", {"default_text": "OK", "roles": roles}, timeout=600)
    assert result.returncode == 0, result.stdout[-3000:]
    _no_notes(result)
    # Only the chosen group's goals were worked.
    heads = {p["head"] for p in harness.read_gh_state()["prs"]}
    assert heads == {"grind/goal-2", "grind/goal-3"}, heads
    # No group got a feature branch: this plan runs no feature setup.
    assert "grind/meta-" not in harness.git("ls-remote", "--heads", "origin")
    for g in ("4", "5"):
        seen = [
            r["role"]
            for r in result.requests
            if r["role"] != "main" and f"Goal {g}:" in _text(r.get("messages"))
        ]
        assert not seen, (g, seen)
        assert not (Path(harness.repo) / f"{g}.txt").exists()
    # The deferred group's goals come back with status `deferred`.
    told = _told(result)
    for g in ("4", "5"):
        pattern = rf'goal\\*"\s*:\s*\\*"{g}\\*".{{0,200}}?status\\*"\s*:\s*\\*"deferred'
        assert re.search(pattern, told, re.DOTALL), told[-3000:]
    assert "deferred group cli" in told, told[-3000:]

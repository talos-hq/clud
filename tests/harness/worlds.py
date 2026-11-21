"""Canned fake-GitHub worlds for the /grind harness tests (#1402).

Each function returns a fresh ``fake_gh`` state dict (write it with
``Harness.write_gh_state``). Native sub-issue links are kept consistent in
both directions: every ``sub_issues`` entry's issue has ``parent`` pointing
back, and every issue with a ``parent`` is listed in that parent's
``sub_issues``.

The builders cover #1392's fixture list: a single-change issue, a
multi-part issue, a meta issue with native sub-issues, a task-list meta
issue, a bare issue list, a mixed bugs+feature meta issue, a regroupable
meta issue (2 groups of 4, 8 children), and meta issues with user-made or
grind-made (marked) sub-meta issues.
"""

from __future__ import annotations

from typing import Any

# The marker /grind writes into the sub-meta issues it creates (with the
# `grind:meta` label); see the /grind skill's regroup step.
GRIND_MARKER = "<!-- grind:v1 -->"
# These suites script the planner, editor, reviewer and integrator roles, so
# their grind-run calls opt into the staged pipeline; the default worker
# pipeline (#2873) is covered offline by tests/test_grind_worker_stacking_2873.py.
STAGED: dict[str, Any] = {"pipeline": "staged"}


def _issue(
    title: str,
    body: str = "",
    *,
    labels: list[str] | None = None,
    parent: int | None = None,
) -> dict[str, Any]:
    return {
        "title": title,
        "body": body,
        "state": "open",
        "labels": list(labels or []),
        "comments": [],
        "sub_issues": [],
        "parent": parent,
    }


def _world(issues: dict[int, dict[str, Any]]) -> dict[str, Any]:
    """Wrap `issues`, filling each parent's `sub_issues` from the children."""
    for number, issue in sorted(issues.items()):
        parent = issue["parent"]
        if parent is not None:
            issues[parent]["sub_issues"].append({"number": number, "state": issue["state"]})
    return {
        "repo": "o/r",
        "default_branch": "main",
        "issues": {str(n): issue for n, issue in issues.items()},
        "prs": [],
        "next_id": 1000,
    }


def single_change() -> dict[str, Any]:
    return _world({1: _issue("fix the typo in README", "README says 'teh'; make it 'the'.")})


def multi_part() -> dict[str, Any]:
    body = (
        "Several parts:\n\n"
        "1. add a --verbose flag\n"
        "2. log each step when verbose\n"
        "3. document the flag in README\n"
    )
    return _world({1: _issue("verbose mode", body)})


def meta_with_sub_issues(n: int = 3) -> dict[str, Any]:
    issues = {1: _issue("meta: cleanup", "Tracked as sub-issues.")}
    for i in range(n):
        issues[2 + i] = _issue(f"cleanup part {i + 1}", f"do part {i + 1}", parent=1)
    return _world(issues)


def task_list_meta() -> dict[str, Any]:
    body = "Task list:\n\n- [ ] #2\n- [ ] #3\n- [ ] #4\n"
    return _world(
        {
            1: _issue("meta: task list", body),
            2: _issue("task a", "do a"),
            3: _issue("task b", "do b"),
            4: _issue("task c", "do c"),
        }
    )


def issue_list(numbers: tuple[int, ...] = (7, 8, 9)) -> dict[str, Any]:
    """Unrelated open issues and no meta issue (``/grind 7 8 9``)."""
    return _world({n: _issue(f"task {n}", f"do {n}") for n in numbers})


def mixed() -> dict[str, Any]:
    """Meta #1 whose native sub-issues are two bugs (#2, #3) and two parts
    of one feature (#4, #5, #5 building on #4). Holds exactly #1-#5."""
    return _world(
        {
            1: _issue("meta: mixed bugs and a feature", "Tracked as sub-issues."),
            2: _issue(
                "crash when the config file is empty",
                "`load_config` raises on an empty file; treat it as defaults.",
                labels=["bug"],
                parent=1,
            ),
            3: _issue(
                "typo in the --help text",
                "`--verbsoe` should read `--verbose`.",
                labels=["bug"],
                parent=1,
            ),
            4: _issue(
                "export: add the export command",
                "Part 1 of the export feature: a new `export` subcommand.",
                labels=["enhancement"],
                parent=1,
            ),
            5: _issue(
                "export: CSV output",
                "Part 2 of the export feature: `export --csv`; builds on #4.",
                labels=["enhancement"],
                parent=1,
            ),
        }
    )


def regroupable() -> dict[str, Any]:
    issues = {1: _issue("meta: backlog", "A flat backlog across two themes.")}
    number = 2
    for theme in ("docs", "cli"):
        for i in range(4):
            issues[number] = _issue(f"{theme}: item {i + 1}", f"{theme} work {i + 1}", parent=1)
            number += 1
    return _world(issues)


def with_user_sub_meta() -> dict[str, Any]:
    return _world(
        {
            1: _issue("meta: top", "Top-level meta."),
            2: _issue("leaf a", "do a", parent=1),
            3: _issue("meta: user-made group", "A group the user made.", parent=1),
            4: _issue("leaf b", "do b", parent=3),
            5: _issue("leaf c", "do c", parent=3),
        }
    )


def with_grind_sub_meta() -> dict[str, Any]:
    return _world(
        {
            1: _issue("meta: top", "Top-level meta."),
            2: _issue("leaf a", "do a", parent=1),
            3: _issue(
                "grind: group",
                f"{GRIND_MARKER}\nGrouped by /grind.",
                labels=["grind:meta"],
                parent=1,
            ),
            4: _issue("leaf b", "do b", parent=3),
            5: _issue("leaf c", "do c", parent=3),
        }
    )


ALL = {
    "single_change": single_change,
    "multi_part": multi_part,
    "meta_with_sub_issues": meta_with_sub_issues,
    "task_list_meta": task_list_meta,
    "issue_list": issue_list,
    "mixed": mixed,
    "regroupable": regroupable,
    "with_user_sub_meta": with_user_sub_meta,
    "with_grind_sub_meta": with_grind_sub_meta,
}

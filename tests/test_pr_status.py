"""pr_status.py answers a multi-PR status sweep in one bounded call (#2233)."""

from __future__ import annotations

import importlib.util
import json
import os
import stat
import sys
from pathlib import Path

import pytest

TOOL = Path(__file__).resolve().parents[1] / "crates/clud-bin/assets/tools/github/pr_status.py"


def _load():
    name = "clud_test_pr_status"
    spec = importlib.util.spec_from_file_location(name, TOOL)
    assert spec
    assert spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


@pytest.fixture
def pr_status():
    return _load()


def pull(number=42, state="OPEN", draft=False, merge="CLEAN", review="APPROVED", checks=()):
    rollup = []
    for kind, name, conclusion in checks:
        if kind == "status":
            rollup.append({"__typename": "StatusContext", "context": name, "state": conclusion})
        else:
            rollup.append(
                {
                    "__typename": "CheckRun",
                    "name": name,
                    "status": "COMPLETED",
                    "conclusion": conclusion,
                }
            )
    return {
        "number": number,
        "state": state,
        "isDraft": draft,
        "mergeStateStatus": merge,
        "reviewDecision": review,
        "title": f"title of {number}",
        "statusCheckRollup": rollup,
    }


STUB = """#!{python}
import json, os, sys
state = json.load(open(os.environ["PR_STATUS_STUB"]))
argv = sys.argv[1:]
number = argv[argv.index("view") + 1] if "view" in argv else ""
record = state.get(number)
if record is None:
    sys.stderr.write("GraphQL: no pull request " + number + ".\\n")
    sys.exit(1)
if isinstance(record, dict) and "error" in record:
    sys.stderr.write(record["error"] + "\\n")
    sys.exit(record.get("code", 1))
print(json.dumps(record))
"""


@pytest.fixture
def stub_gh(tmp_path, monkeypatch):
    bindir = tmp_path / "bin"
    bindir.mkdir()
    script = bindir / "gh"
    script.write_text(STUB.format(python=sys.executable))
    script.chmod(script.stat().st_mode | stat.S_IEXEC)
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}{os.environ['PATH']}")
    path = tmp_path / "pulls.json"

    def make(records: dict) -> Path:
        path.write_text(json.dumps(records))
        monkeypatch.setenv("PR_STATUS_STUB", str(path))
        return path

    return make


def test_one_line_per_pr_with_counts_and_failing_names(pr_status, stub_gh, capsys):
    stub_gh(
        {
            "3228": pull(
                3228,
                draft=True,
                merge="BEHIND",
                review="REVIEW_REQUIRED",
                checks=[("run", "build", "SUCCESS"), ("run", "ci-scipy", "FAILURE")],
            ),
            "3244": pull(3244, checks=[("run", "build", "SUCCESS")]),
        }
    )
    code = pr_status.main(["--repo", "o/r", "--pr", "3228", "3244"])
    out = capsys.readouterr().out.splitlines()
    assert code == 0
    assert out[0] == (
        "#3228 OPEN(draft) merge=BEHIND review=REVIEW_REQUIRED checks=1/2 fail=ci-scipy"
    )
    assert out[1] == "#3244 OPEN merge=CLEAN review=APPROVED checks=1/1"
    assert out[2] == "2 PR(s) from o/r: 2 read, 1 failing"


def test_a_check_name_list_is_capped_and_names_its_continuation(pr_status, stub_gh, capsys):
    names = [("run", f"job-{i}", "FAILURE") for i in range(5)]
    stub_gh({"7": pull(7, checks=names)})
    code = pr_status.main(["--repo", "o/r", "--pr", "7", "--checks", "1"])
    line = capsys.readouterr().out.splitlines()[0]
    assert code == 0
    assert "fail=job-0 +4 (--checks 5)" in line


def test_status_context_failures_and_pending_are_counted(pr_status, stub_gh, capsys):
    stub_gh(
        {
            "1": pull(
                1,
                checks=[
                    ("status", "ci/circleci", "FAILURE"),
                    ("run", "lint", "IN_PROGRESS"),
                    ("run", "doc", "SKIPPED"),
                ],
            )
        }
    )
    pr_status.main(["--repo", "o/r", "--pr", "1"])
    line = capsys.readouterr().out.splitlines()[0]
    assert "checks=1/3" in line
    assert "fail=ci/circleci" in line
    assert "pending=1" in line


def test_limit_reports_what_it_did_not_read(pr_status, stub_gh, capsys):
    stub_gh({str(n): pull(n) for n in (1, 2, 3)})
    code = pr_status.main(["--repo", "o/r", "--pr", "1", "2", "3", "--limit", "1"])
    out = capsys.readouterr().out.splitlines()
    assert code == 0
    assert len(out) == 2
    assert out[1] == "3 PR(s) from o/r: 1 read, 0 failing, +2 not read (--limit)"


def test_an_unreadable_pr_is_named_and_exits_two(pr_status, stub_gh, capsys):
    stub_gh({"1": pull(1), "2": {"error": "HTTP 404: Not Found"}})
    code = pr_status.main(["--repo", "o/r", "--pr", "1", "2"])
    out = capsys.readouterr().out.splitlines()
    assert code == 2
    assert out[1].startswith("#2 unreachable (gh exit 1: HTTP 404: Not Found)")
    assert out[2].endswith("1 read, 0 failing, 1 unreachable")


def test_merged_pr_does_not_print_a_merge_state(pr_status, stub_gh, capsys):
    stub_gh({"9": pull(9, state="MERGED", merge="UNKNOWN", review="", checks=[])})
    pr_status.main(["--repo", "o/r", "--pr", "9"])
    line = capsys.readouterr().out.splitlines()[0]
    assert line.startswith("#9 MERGED merge=- review=- checks=0/0")


def test_title_is_opt_in(pr_status, stub_gh, capsys):
    stub_gh({"5": pull(5)})
    pr_status.main(["--repo", "o/r", "--pr", "5"])
    assert "title of 5" not in capsys.readouterr().out
    pr_status.main(["--repo", "o/r", "--pr", "5", "--title"])
    assert "title of 5" in capsys.readouterr().out


def test_page_stays_within_the_byte_cap(pr_status, stub_gh, capsys):
    names = [("run", f"{i:02d}-" + "x" * 60, "FAILURE") for i in range(40)]
    stub_gh({str(n): pull(n, checks=names) for n in range(1, 31)})
    code = pr_status.main(
        ["--repo", "o/r", "--pr", *[str(n) for n in range(1, 31)], "--checks", "9", "--limit", "30"]
    )
    text = capsys.readouterr().out
    assert code == 0
    assert len(text.encode()) <= pr_status.MAX_OUTPUT + 400
    assert "page cap" in text


def test_parse_prs_accepts_both_spellings(pr_status):
    assert pr_status.parse_prs([["1", "2"]]) == ([1, 2], None)
    assert pr_status.parse_prs([["1,2"]]) == ([1, 2], None)
    assert pr_status.parse_prs([["1"], ["1", "3"]]) == ([1, 3], None)
    numbers, error = pr_status.parse_prs([["1", "abc"]])
    assert numbers == []
    assert "abc" in error
    assert pr_status.parse_prs([])[1] == "no --pr given"


def test_repo_is_required_when_nothing_resolves(pr_status, monkeypatch):
    monkeypatch.setattr(pr_status, "resolve_repo", lambda explicit: None)
    assert pr_status.main(["--pr", "1"]) == 1

"""Unit tests for the fake `gh` GitHub semantics used by /grind tests (#1402)."""

from __future__ import annotations

import io
import json
import sys

import pytest

from tests.harness import fake_gh


@pytest.fixture
def world(tmp_path, monkeypatch):
    path = tmp_path / "gh.json"

    def make(state: dict | None = None):
        path.write_text(json.dumps(state or {"repo": "o/r", "issues": {}, "prs": []}))
        monkeypatch.setenv("FAKE_GH_STATE", str(path))
        return path

    make.path = path
    return make


def _state(world) -> dict:
    return json.loads(world.path.read_text())


def _run(capsys, *argv: str) -> tuple[int, str, str]:
    capsys.readouterr()
    code = fake_gh.main(list(argv))
    out = capsys.readouterr()
    return code, out.out, out.err


def test_legacy_state_shape_still_works(world, capsys):
    world(
        {
            "repo": "o/r",
            "issues": {
                "7": {
                    "title": "Meta",
                    "body": "b",
                    "state": "open",
                    "sub_issues": [{"number": 8, "state": "closed"}],
                }
            },
            "prs": [{"number": 1, "head": "feat/x", "state": "MERGED", "title": "t"}],
            "fail": False,
            "calls": [],
        }
    )
    code, out, _ = _run(capsys, "issue", "view", "7", "--json", "title,body,state")
    assert code == 0
    data = json.loads(out)
    assert (data["number"], data["title"], data["state"]) == (7, "Meta", "OPEN")
    code, out, _ = _run(capsys, "api", "repos/o/r/issues/7/sub_issues")
    assert code == 0
    assert [(s["number"], s["state"]) for s in json.loads(out)] == [(8, "closed")]
    code, out, _ = _run(capsys, "pr", "list", "--head", "feat/x", "--state", "all")
    assert json.loads(out)[0]["number"] == 1
    assert len(_state(world)["calls"]) == 3


def test_create_comment_labels_and_list(world, capsys):
    world()
    code, out, _ = _run(capsys, "issue", "create", "--title", "A", "--body", "x", "--label", "bug")
    assert code == 0
    assert out.strip() == "https://github.com/o/r/issues/1"
    _run(capsys, "issue", "create", "--title", "B", "--body", "y", "--label", "a,b")
    code, out, _ = _run(capsys, "issue", "comment", "1", "--body", "hi")
    first = int(out.strip().rsplit("#issuecomment-", 1)[1])
    code, out, _ = _run(capsys, "issue", "comment", "1", "--body", "again")
    second = int(out.strip().rsplit("#issuecomment-", 1)[1])
    assert second != first

    _run(capsys, "issue", "edit", "1", "--add-label", "x,y", "--remove-label", "bug")
    code, out, _ = _run(capsys, "issue", "view", "1", "--json", "labels,comments")
    data = json.loads(out)
    assert [lbl["name"] for lbl in data["labels"]] == ["x", "y"]
    assert data["comments"] == [{"id": first, "body": "hi"}, {"id": second, "body": "again"}]

    code, out, _ = _run(capsys, "issue", "list", "--label", "a", "--json", "number")
    assert [i["number"] for i in json.loads(out)] == [2]
    _run(capsys, "issue", "close", "2")
    _, out, _ = _run(capsys, "issue", "list", "--state", "open", "--json", "number")
    assert [i["number"] for i in json.loads(out)] == [1]
    _, out, _ = _run(capsys, "issue", "list", "--state", "closed", "--json", "number")
    rows = json.loads(out)
    assert rows[0]["number"] == 2
    assert rows[0]["state"] == "CLOSED"
    _, out, _ = _run(capsys, "issue", "list", "--state", "all", "--json", "number")
    assert len(json.loads(out)) == 2


def test_close_and_reopen_record_closer(world, capsys):
    world({"issues": {"3": {"title": "t", "body": "", "state": "open"}}})
    code, _, _ = _run(capsys, "issue", "close", "3", "--comment", "done")
    assert code == 0
    issue = _state(world)["issues"]["3"]
    assert issue["state"] == "closed"
    assert issue["closed_by"]["kind"] == "command"
    assert issue["comments"][0]["body"] == "done"
    _run(capsys, "issue", "reopen", "3")
    issue = _state(world)["issues"]["3"]
    assert issue["state"] == "open"
    assert issue["closed_by"] is None


def test_comment_patch_and_get(world, capsys):
    world({"issues": {"1": {"title": "t", "body": "", "state": "open"}}})
    _, out, _ = _run(capsys, "issue", "comment", "1", "--body", "old")
    cid = out.strip().rsplit("-", 1)[1]
    code, _, _ = _run(
        capsys, "api", "-X", "PATCH", f"repos/o/r/issues/comments/{cid}", "-f", "body=new"
    )
    assert code == 0
    code, out, _ = _run(capsys, "api", f"repos/o/r/issues/comments/{cid}")
    assert code == 0
    assert json.loads(out)["body"] == "new"


def _issues(n: int) -> dict:
    return {str(i): {"title": f"i{i}", "body": "", "state": "open"} for i in range(1, n + 1)}


def _add(capsys, parent: int, child: int, *extra: str):
    return _run(
        capsys,
        "api",
        "-X",
        "POST",
        f"repos/o/r/issues/{parent}/sub_issues",
        "-F",
        f"sub_issue_id={child}",
        *extra,
    )


def test_sub_issue_post_replace_one_parent_and_delete(world, capsys):
    world({"issues": _issues(3)})
    assert _add(capsys, 1, 3)[0] == 0
    code, _, err = _add(capsys, 2, 3)
    assert code == 1
    assert "422" in err
    assert _add(capsys, 2, 3, "-F", "replace_parent=true")[0] == 0
    state = _state(world)
    assert state["issues"]["1"]["sub_issues"] == []
    assert [s["number"] for s in state["issues"]["2"]["sub_issues"]] == [3]
    assert state["issues"]["3"]["parent"] == 2
    _, out, _ = _run(capsys, "api", "repos/o/r/issues/2/sub_issues")
    assert [s["number"] for s in json.loads(out)] == [3]
    code, _, _ = _run(
        capsys, "api", "-X", "DELETE", "repos/o/r/issues/2/sub_issue", "-F", "sub_issue_id=3"
    )
    assert code == 0
    state = _state(world)
    assert state["issues"]["2"]["sub_issues"] == []
    assert state["issues"]["3"]["parent"] is None


def test_sub_issue_hundred_limit(world, capsys):
    issues = _issues(102)
    issues["1"]["sub_issues"] = [{"number": i, "state": "open"} for i in range(2, 102)]
    for i in range(2, 102):
        issues[str(i)]["parent"] = 1
    world({"issues": issues})
    code, _, err = _add(capsys, 1, 102)
    assert code == 1
    assert "422" in err


def test_sub_issue_eight_level_limit(world, capsys):
    world({"issues": _issues(9)})
    for level in range(1, 8):
        assert _add(capsys, level, level + 1)[0] == 0
    code, _, err = _add(capsys, 8, 9)
    assert code == 1
    assert "422" in err


def test_pr_create_draft_ready_edit(world, capsys):
    world()
    _code, out, _ = _run(
        capsys, "pr", "create", "--head", "f", "--title", "t", "--body", "b", "--draft"
    )
    number = out.strip().rsplit("/", 1)[1]
    _, out, _ = _run(capsys, "pr", "view", number, "--json", "isDraft,baseRefName,body")
    data = json.loads(out)
    assert data["isDraft"] is True
    assert data["baseRefName"] == "main"
    assert data["body"] == "b"
    assert _run(capsys, "pr", "merge", number, "--squash")[0] == 1
    _run(capsys, "pr", "ready", number)
    _run(capsys, "pr", "edit", number, "--body", "b2")
    _, out, _ = _run(capsys, "pr", "view", number, "--json", "isDraft,body")
    assert json.loads(out)["isDraft"] is False
    assert json.loads(out)["body"] == "b2"


def test_merge_records_method_and_admin_and_requires_review(world, capsys):
    world(
        {
            "issues": {},
            "prs": [
                {"number": 1, "head": "a", "state": "OPEN", "title": "t"},
                {
                    "number": 2,
                    "head": "b",
                    "state": "OPEN",
                    "title": "t",
                    "reviews_required": True,
                    "approved": False,
                },
            ],
        }
    )
    assert _run(capsys, "pr", "merge", "1", "--rebase")[0] == 0
    pr = _state(world)["prs"][0]
    assert (pr["state"], pr["merge_method"], pr["admin"]) == ("MERGED", "rebase", False)
    assert _run(capsys, "pr", "merge", "2", "--squash")[0] == 1
    assert _state(world)["prs"][1]["state"] == "OPEN"
    assert _run(capsys, "pr", "merge", "2", "--squash", "--admin")[0] == 0
    pr = _state(world)["prs"][1]
    assert (pr["merge_method"], pr["admin"]) == ("squash", True)


def test_closing_keyword_only_on_default_branch(world, capsys):
    world(
        {
            "issues": {
                "5": {"title": "t", "body": "", "state": "open"},
                "6": {"title": "t", "body": "", "state": "open"},
            },
            "prs": [],
        }
    )
    _run(capsys, "pr", "create", "--head", "f", "--title", "x", "--body", "Closes #5")
    _run(capsys, "pr", "create", "--base", "feat", "--head", "g", "--title", "fixes #6")
    _run(capsys, "pr", "merge", "101", "--squash")
    _run(capsys, "pr", "merge", "102", "--squash")
    issues = _state(world)["issues"]
    assert issues["5"]["state"] == "closed"
    assert issues["5"]["closed_by"] == {"kind": "pr", "pr": 101}
    assert issues["6"]["state"] == "open"

    query = "query { repository { issue(number: 5) { timelineItems(itemTypes: CLOSED_EVENT) "
    query += "{ nodes { ... on ClosedEvent { closer { __typename } } } } } } }"
    _, out, _ = _run(capsys, "api", "graphql", "-f", f"query={query}")
    nodes = json.loads(out)["data"]["repository"]["issue"]["timelineItems"]["nodes"]
    # The closer carries what reconcile (#1393) needs to judge it legitimate.
    closer = {"__typename": "PullRequest", "number": 101, "merged": True, "baseRefName": "main"}
    assert nodes == [{"closer": closer}]
    _run(capsys, "issue", "close", "6")
    _, out, _ = _run(capsys, "api", "graphql", "-f", f"query={query}", "-F", "number=6")
    nodes = json.loads(out)["data"]["repository"]["issue"]["timelineItems"]["nodes"]
    assert nodes == [{"closer": None}]


def test_fault_injection_consumed_by_times(world, capsys):
    world(
        {
            "issues": {"1": {"title": "t", "body": "", "state": "open"}},
            "faults": {"issue close": {"code": 1, "stderr": "boom", "times": 1}},
        }
    )
    code, _, err = _run(capsys, "issue", "close", "1")
    assert code == 1
    assert "boom" in err
    assert _state(world)["issues"]["1"]["state"] == "open"
    assert _run(capsys, "issue", "close", "1")[0] == 0
    assert _state(world)["issues"]["1"]["state"] == "closed"
    assert len(_state(world)["calls"]) == 2


def test_api_method_fault_key(world, capsys):
    world({"issues": {}, "faults": {"api PATCH": {"code": 2, "stderr": "nope", "times": 1}}})
    code, _, err = _run(capsys, "api", "-X", "PATCH", "repos/o/r/issues/comments/1")
    assert code == 2
    assert "nope" in err


def test_body_limit_65536(world, capsys):
    world({"issues": {"1": {"title": "t", "body": "", "state": "open"}}})
    big = "x" * 65537
    code, _, err = _run(capsys, "issue", "comment", "1", "--body", big)
    assert code == 1
    assert "65536" in err
    code, _, err = _run(capsys, "issue", "create", "--title", "t", "--body", big)
    assert code == 1
    assert "65536" in err
    code, _, err = _run(capsys, "pr", "create", "--head", "h", "--title", "t", "--body", big)
    assert code == 1
    assert "65536" in err
    assert _run(capsys, "issue", "comment", "1", "--body", "x" * 65536)[0] == 0


def _one(state: dict | None = None) -> dict:
    return {"issues": {"1": {"title": "t", "body": "b", "state": "open"}}, **(state or {})}


def test_rest_issue_get_patch_and_comments(world, capsys):
    world(_one())
    code, out, _ = _run(capsys, "api", "repos/{owner}/{repo}/issues/1")
    assert code == 0
    data = json.loads(out)
    assert (data["id"], data["number"], data["body"], data["state"]) == (1, 1, "b", "open")
    code, out, _ = _run(capsys, "api", "repos/o/r/issues/1/comments", "-f", "body=hello")
    assert code == 0
    cid = json.loads(out)["id"]
    _, out, _ = _run(capsys, "api", "repos/o/r/issues/1/comments")
    assert [c["body"] for c in json.loads(out)] == ["hello"]
    assert _run(capsys, "api", "-X", "DELETE", f"repos/o/r/issues/comments/{cid}")[0] == 0
    assert _state(world)["issues"]["1"]["comments"] == []
    _run(capsys, "api", "-X", "PATCH", "repos/o/r/issues/1", "-f", "state=closed")
    issue = _state(world)["issues"]["1"]
    assert issue["state"] == "closed"
    assert issue["closed_by"]["kind"] == "user"
    assert _run(capsys, "api", "repos/o/r/issues/9")[0] == 1


def test_api_endpoint_after_input_flag(world, capsys, tmp_path):
    world(_one())
    _, out, _ = _run(capsys, "issue", "comment", "1", "--body", "old")
    cid = out.strip().rsplit("-", 1)[1]
    body = tmp_path / "sub" / "body.json"
    body.parent.mkdir()
    body.write_text(json.dumps({"body": "new"}))
    args = ("api", "--input", str(body), "-X", "PATCH", f"repos/o/r/issues/comments/{cid}")
    assert _run(capsys, *args)[0] == 0
    assert _state(world)["issues"]["1"]["comments"][0]["body"] == "new"


def test_jq_paths(world, capsys):
    world(_one({"prs": [{"number": 150, "head": "h", "state": "OPEN", "title": "t"}]}))
    _, out, _ = _run(capsys, "pr", "view", "150", "--json", "state", "-q", ".state")
    assert out.strip() == "OPEN"
    _, out, _ = _run(capsys, "issue", "list", "--json", "number", "--jq", ".[].number")
    assert out.split() == ["1"]
    _, out, _ = _run(capsys, "issue", "list", "--jq", "length")
    assert out.strip() == "1"
    _, out, _ = _run(capsys, "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner")
    assert out.strip() == "o/r"
    code, _, err = _run(capsys, "issue", "list", "--jq", "map(.number)")
    assert code == 1
    assert "unsupported" in err


def test_body_file_and_stdin(world, capsys, tmp_path, monkeypatch):
    world(_one({"prs": [{"number": 101, "head": "f", "state": "OPEN", "title": "t"}]}))
    body = tmp_path / "body.md"
    body.write_text("from a file")
    _, out, _ = _run(capsys, "issue", "create", "--title", "x", "--body-file", str(body))
    # Issue numbers skip past PR numbers, as GitHub's shared numbering does.
    number = out.strip().rsplit("/", 1)[1]
    assert number == "102"
    assert _state(world)["issues"][number]["body"] == "from a file"
    _run(capsys, "issue", "edit", number, "--body-file", str(body))
    _run(capsys, "pr", "edit", "101", "--body-file", str(body))
    assert _state(world)["prs"][0]["body"] == "from a file"
    monkeypatch.setattr(sys, "stdin", io.StringIO("from stdin"))
    _run(capsys, "issue", "comment", "1", "--repo", "o/r", "--body-file", "-")
    assert _state(world)["issues"]["1"]["comments"][0]["body"] == "from stdin"


def test_issue_list_search(world, capsys):
    world(
        {
            "issues": {
                "1": {"title": "flaky cache test", "body": "x", "state": "open", "labels": ["f"]},
                "2": {"title": "other", "body": "cache", "state": "closed", "labels": ["f"]},
                "3": {"title": "cache again", "body": "", "state": "open", "labels": ["g"]},
            }
        }
    )

    def numbers(*extra: str) -> list[int]:
        _, out, _ = _run(capsys, "issue", "list", "--json", "number", *extra)
        return [i["number"] for i in json.loads(out)]

    assert numbers("--state", "all", "--label", "f", "--search", "cache in:title") == [1]
    assert numbers("--state", "all", "--search", "cache") == [1, 2, 3]
    assert numbers("--search", "-label:g") == [1]
    assert numbers("--search", "is:closed cache") == [2]
    assert numbers("--state", "all", "--limit", "2") == [1, 2]


def test_pr_found_by_head_even_when_it_ends_in_a_number(world, capsys):
    world(
        {
            "issues": {},
            "prs": [
                {"number": 101, "head": "grind/meta-1", "state": "OPEN", "title": "f"},
                {"number": 102, "head": "grind/103", "state": "OPEN", "title": "g"},
            ],
        }
    )
    assert _run(capsys, "pr", "merge", "grind/103", "--merge")[0] == 0
    assert _state(world)["prs"][1]["state"] == "MERGED"
    _, out, _ = _run(capsys, "pr", "view", "https://github.com/o/r/pull/101", "--json", "number")
    assert json.loads(out)["number"] == 101
    _, out, _ = _run(capsys, "pr", "view", "#101", "--json", "number")
    assert json.loads(out)["number"] == 101


def test_pr_current_branch_from_git_head(world, capsys, tmp_path, monkeypatch):
    world({"issues": {}, "prs": [{"number": 101, "head": "feat/y", "state": "OPEN", "body": "b"}]})
    monkeypatch.delenv("FAKE_GH_HEAD", raising=False)
    repo = tmp_path / "repo"
    (repo / ".git").mkdir(parents=True)
    (repo / ".git" / "HEAD").write_text("ref: refs/heads/feat/y\n")
    monkeypatch.chdir(repo)
    _, out, _ = _run(capsys, "pr", "view", "--json", "body")
    assert json.loads(out)["body"] == "b"
    # A linked worktree's `.git` is a file naming its gitdir.
    gitdir = tmp_path / "gitdir"
    gitdir.mkdir()
    (gitdir / "HEAD").write_text("ref: refs/heads/feat/z\n")
    wt = tmp_path / "wt"
    wt.mkdir()
    (wt / ".git").write_text(f"gitdir: {gitdir}\n")
    monkeypatch.chdir(wt)
    _run(capsys, "pr", "create", "--title", "z", "--body", "")
    assert _state(world)["prs"][-1]["head"] == "feat/z"


def test_pr_list_filters_and_fields(world, capsys):
    world(
        {
            "issues": {},
            "prs": [
                {"number": 101, "head": "grind/meta-5-a", "state": "OPEN", "base": "main"},
                {"number": 102, "head": "grind/7", "state": "OPEN", "base": "grind/meta-5-a"},
                {"number": 103, "head": "x", "state": "MERGED", "base": "main", "draft": False},
            ],
        }
    )

    def numbers(*extra: str) -> list[int]:
        _, out, _ = _run(capsys, "pr", "list", "--json", "number", *extra)
        return [p["number"] for p in json.loads(out)]

    assert numbers("--search", "head:grind/meta-5-") == [101]
    assert numbers("--base", "grind/meta-5-a") == [102]
    assert numbers("--state", "all", "--base", "main") == [101, 103]
    _, out, _ = _run(capsys, "pr", "list", "--head", "grind/7", "--json", "isDraft,baseRefName")
    row = json.loads(out)[0]
    assert (row["isDraft"], row["baseRefName"]) == (False, "grind/meta-5-a")


def test_pr_create_refuses_a_duplicate_and_merge_needs_a_method(world, capsys):
    world()
    assert _run(capsys, "pr", "create", "--head", "f", "--title", "t")[0] == 0
    code, _, err = _run(capsys, "pr", "create", "--head", "f", "--title", "t")
    assert code == 1
    assert "already exists" in err
    code, _, err = _run(capsys, "pr", "merge", "101")
    assert code == 1
    assert "--squash" in err
    assert _run(capsys, "pr", "merge", "101", "--squash")[0] == 0
    assert _state(world)["prs"][0]["merged_at"]
    code, _, err = _run(capsys, "pr", "merge", "101", "--squash")
    assert code == 1
    assert "already merged" in err
    assert _run(capsys, "pr", "ready", "101", "--undo")[0] == 0


def test_repo_level_required_review(world, capsys):
    world(
        {
            "issues": {},
            "requires_review": True,
            "prs": [
                {"number": 1, "head": "a", "state": "OPEN", "base": "main"},
                {"number": 2, "head": "b", "state": "OPEN", "base": "feature"},
                {"number": 3, "head": "c", "state": "OPEN", "base": "main", "approved": True},
            ],
        }
    )
    code, _, err = _run(capsys, "pr", "merge", "1", "--merge")
    assert code == 1
    assert "review required" in err
    _, out, _ = _run(capsys, "pr", "view", "1", "--json", "reviewDecision")
    assert json.loads(out)["reviewDecision"] == "REVIEW_REQUIRED"
    assert _run(capsys, "pr", "merge", "2", "--merge")[0] == 0
    assert _run(capsys, "pr", "merge", "3", "--merge")[0] == 0
    assert _run(capsys, "pr", "merge", "1", "--merge", "--admin")[0] == 0


def test_closing_rule_reads_the_body_and_keeps_an_earlier_closer(world, capsys):
    world(
        {
            "issues": {
                "5": {"title": "t", "body": "", "state": "open"},
                "6": {"title": "t", "body": "", "state": "open"},
                "7": {"title": "t", "body": "", "state": "open"},
            },
            "prs": [],
        }
    )
    _run(capsys, "issue", "close", "7")
    body = "Resolves #5\nFixes: o/r#7\nCloses other/repo#6"
    _run(capsys, "pr", "create", "--head", "f", "--title", "Closes #6", "--body", body)
    _run(capsys, "pr", "merge", "101", "--merge")
    issues = _state(world)["issues"]
    assert issues["5"]["closed_by"] == {"kind": "pr", "pr": 101}
    # The title and another repo's reference close nothing.
    assert issues["6"]["state"] == "open"
    # An issue closed earlier keeps its closer.
    assert issues["7"]["closed_by"]["kind"] == "command"


def test_graphql_commit_closer(world, capsys):
    world(
        {
            "issues": {
                "4": {
                    "title": "t",
                    "body": "",
                    "state": "closed",
                    "closed_by": {"kind": "commit", "pr": None, "oid": "abc"},
                }
            }
        }
    )
    query = "query { repository { issue(number: 4) { timelineItems { nodes { closer } } } } }"
    _, out, _ = _run(capsys, "api", "graphql", "-f", f"query={query}")
    nodes = json.loads(out)["data"]["repository"]["issue"]["timelineItems"]["nodes"]
    assert nodes == [{"closer": {"__typename": "Commit", "oid": "abc"}}]


def test_close_and_reopen_are_idempotent(world, capsys):
    world(_one())
    _run(capsys, "issue", "close", "1")
    code, _, err = _run(capsys, "issue", "close", "1", "--comment", "again")
    assert code == 0
    assert "already closed" in err
    assert _state(world)["issues"]["1"].get("comments", []) == []
    _run(capsys, "issue", "reopen", "1")
    code, _, err = _run(capsys, "issue", "reopen", "1")
    assert code == 0
    assert "already open" in err


def test_sub_issue_cycle_duplicate_parent_get_and_live_state(world, capsys):
    world({"issues": _issues(3)})
    assert _add(capsys, 1, 2)[0] == 0
    assert _add(capsys, 2, 3)[0] == 0
    code, _, err = _add(capsys, 3, 1)
    assert code == 1
    assert "422" in err
    code, _, err = _add(capsys, 1, 2)
    assert code == 1
    assert "duplicate" in err
    assert _add(capsys, 1, 2, "-F", "replace_parent=true")[0] == 0
    _, out, _ = _run(capsys, "api", "repos/o/r/issues/3/parent")
    assert json.loads(out)["number"] == 2
    assert _run(capsys, "api", "repos/o/r/issues/1/parent")[0] == 1
    _run(capsys, "issue", "close", "2")
    _, out, _ = _run(capsys, "api", "repos/o/r/issues/1/sub_issues")
    assert [(s["number"], s["state"]) for s in json.loads(out)] == [(2, "closed")]
    _, out, _ = _run(capsys, "api", "repos/o/r/issues/1")
    assert json.loads(out)["sub_issues_summary"]["completed"] == 1


def test_fault_after_narrow_key_and_fail_on(world, capsys):
    world(
        _one(
            {
                "faults": {
                    "issue create": {"code": 1, "stderr": "502", "after": 1, "times": 1},
                    "issue comment 1": {"code": 3, "stderr": "boom"},
                },
                "fail_on": ["api POST repos/o/r/issues/1/sub_issues"],
            }
        )
    )
    assert _run(capsys, "issue", "create", "--title", "a")[0] == 0
    assert _run(capsys, "issue", "create", "--title", "b")[0] == 1
    assert _run(capsys, "issue", "create", "--title", "c")[0] == 0
    assert _run(capsys, "issue", "comment", "1", "--body", "x")[0] == 3
    assert _run(capsys, "issue", "comment", "2", "--body", "x")[0] == 0
    assert _add(capsys, 1, 2)[0] == 1
    assert sorted(_state(world)["issues"]) == ["1", "2", "3"]


def test_body_file_from_stdin_and_path(world, capsys, monkeypatch, tmp_path):
    # /grind-prework pipes the plan into `--body-file -` (#1408);
    # the router may file a follow-up with `--body-file <f>` (#1411).
    world({"issues": {"1": {"title": "t", "body": "", "state": "open"}}})
    plan = "<!-- grind:v1 plan run=r -->\n~~~json\n{}\n~~~\n"
    monkeypatch.setattr("sys.stdin", io.StringIO(plan))
    assert _run(capsys, "issue", "comment", "1", "--body-file", "-")[0] == 0
    body = tmp_path / "followup.md"
    body.write_text("evidence\nRefs #1\n", encoding="utf-8")
    code, out, _ = _run(capsys, "issue", "create", "--title", "x", "-F", str(body))
    assert code == 0
    state = _state(world)
    assert state["issues"]["1"]["comments"][0]["body"] == plan
    number = out.strip().rsplit("/", 1)[-1]
    assert state["issues"][number]["body"] == "evidence\nRefs #1\n"


def _two_repositories() -> dict:
    return {
        "repo": "o/primary",
        "repositories": {
            name: {"repo": name, "default_branch": branch, "prs": [], "issues": {
                "7": {"title": name, "body": marker, "state": "open", "comments": []},
            }}
            for name, branch, marker in [
                ("o/primary", "master", "PRIMARY_REPO_ISSUE"),
                ("o/sister", "main", "SISTER_REPO_ISSUE"),
            ]
        },
    }


@pytest.mark.parametrize("selection", [
    ["issue", "view", "7", "--repo", "o/sister"],
    ["issue", "view", "7", "--repo=o/sister"],
    ["-R", "o/sister", "issue", "view", "7"],
    ["-Ro/sister", "issue", "view", "7"],
    ["issue", "view", "https://github.com/o/sister/issues/7"],
])
def test_repo_selection_does_not_conflate_issue_numbers(world, capsys, selection):
    world(_two_repositories())
    code, out, _ = _run(capsys, *selection, "--json", "title,body,url")
    assert code == 0
    value = json.loads(out)
    assert value["body"] == "SISTER_REPO_ISSUE"
    assert value["url"] == "https://github.com/o/sister/issues/7"


def test_repository_mutations_preserve_other_repositories(world, capsys):
    world(_two_repositories())
    argv = ["issue", "comment", "7", "--repo", "o/sister", "--body", "sister only"]
    code, out, _ = _run(capsys, *argv)
    assert code == 0
    assert out.startswith("https://github.com/o/sister/issues/7#")
    state = _state(world)
    assert state["repositories"]["o/primary"]["issues"]["7"]["comments"] == []
    comments = state["repositories"]["o/sister"]["issues"]["7"]["comments"]
    assert comments[0]["body"] == "sister only"
    assert state["calls"] == [argv]


def test_rest_endpoints_select_their_repository(world, capsys):
    world(_two_repositories())
    code, out, _ = _run(capsys, "api", "repos/o/sister/issues/7", "--jq", ".body")
    assert code == 0
    assert out.strip() == "SISTER_REPO_ISSUE"
    code, out, _ = _run(capsys, "repo", "view", "o/primary", "--json", "defaultBranchRef")
    assert code == 0
    assert json.loads(out)["defaultBranchRef"]["name"] == "master"


def test_unknown_repository_does_not_fall_back_to_primary(world, capsys):
    world(_two_repositories())
    code, _, error = _run(capsys, "issue", "view", "7", "--repo", "o/unknown")
    assert code != 0
    assert "unknown fixture repository o/unknown" in error
    state = _state(world)
    assert len(state["calls"]) == 1
    assert state["repositories"] == _two_repositories()["repositories"]


def test_missing_release_is_a_failed_check(world, capsys):
    world()
    code, _, error = _run(capsys, "release", "view", "v0.9.0", "--repo", "o/r")
    assert code == 1
    assert "release not found" in error
    assert _state(world).get("releases", {}) == {}


def test_release_creation_and_lookup_are_repository_scoped(world, capsys):
    world(_two_repositories())
    code, url, _ = _run(capsys, "release", "create", "v0.9.0", "--repo", "o/sister")
    assert code == 0
    assert url.strip() == "https://github.com/o/sister/releases/tag/v0.9.0"
    code, out, _ = _run(
        capsys, "release", "view", "v0.9.0", "--repo", "o/sister", "--json", "tagName,publishedAt",
    )
    release = json.loads(out)
    assert code == 0
    assert release["tagName"] == "v0.9.0"
    assert release["publishedAt"]
    assert _run(capsys, "release", "view", "v0.9.0", "--repo", "o/primary")[0] == 1
    assert not _state(world)["repositories"]["o/primary"].get("releases")
    code, out, _ = _run(capsys, "api", "repos/o/sister/releases/tags/v0.9.0")
    assert code == 0
    assert json.loads(out)["tag_name"] == "v0.9.0"
    code, out, _ = _run(capsys, "api", "repos/o/primary/releases")
    assert code == 0
    assert json.loads(out) == []


def test_draft_release_lookup_does_not_claim_publication(world, capsys):
    world()
    assert _run(capsys, "release", "create", "v-next", "--draft")[0] == 0
    code, out, _ = _run(capsys, "release", "view", "v-next", "--json", "isDraft,publishedAt")
    release = json.loads(out)
    assert code == 0
    assert release["isDraft"] is True
    assert release["publishedAt"] is None


def test_release_can_appear_after_controlled_missing_observations(world, capsys):
    world({"repo": "o/r", "release_after_views": {"v-next": {
        "after": 2, "release": {"publishedAt": "2026-10-22T00:00:00Z"},
    }}})
    for _ in range(2):
        assert _run(capsys, "release", "view", "v-next")[0] == 1
    assert _run(capsys, "repo", "view", "--json", "nameWithOwner")[0] == 0
    code, out, _ = _run(capsys, "release", "view", "v-next")
    assert code == 0
    assert json.loads(out)["publishedAt"] == "2026-10-22T00:00:00Z"


@pytest.mark.parametrize("args", [
    ["release", "view", "v-next", "--repo", "o/sister"],
    ["release", "view", "--repo", "o/sister", "v-next"],
    ["release", "view", "-Ro/sister", "v-next"],
    ["--repo", "o/sister", "release", "view", "v-next"],
])
def test_release_lookup_accepts_repository_flag_positions(world, capsys, args):
    state = _two_repositories()
    state["repositories"]["o/sister"]["releases"] = {"v-next": {"publishedAt": "published"}}
    world(state)
    code, out, _ = _run(capsys, *args)
    assert code == 0
    assert json.loads(out)["tagName"] == "v-next"
    assert json.loads(out)["url"] == "https://github.com/o/sister/releases/tag/v-next"


def test_do_preflight_reads_are_complete_and_read_only(world, capsys):
    comments = [{"id": n, "body": "Claiming: fix", "author": "other"} for n in range(1, 102)]
    world(
        {
            "repo": "o/r",
            "login": "me",
            "labels": ["ready"],
            "issues": {
                "7": {"state": "closed", "assignees": ["other"], "comments": comments},
            },
            "prs": [],
        }
    )
    code, out, _ = _run(
        capsys, "issue", "view", "7", "--repo", "o/r", "--json", "state,assignees,labels"
    )
    assert code == 0
    assert json.loads(out)["state"] == "CLOSED"
    assert json.loads(out)["assignees"] == [{"login": "other"}]
    code, out, _ = _run(
        capsys, "api", "--paginate", "--slurp", "repos/o/r/issues/7/comments?per_page=100"
    )
    assert code == 0
    assert len(json.loads(out)[0]) == 101
    assert json.loads(out)[0][-1]["user"] == {"login": "other"}
    assert _run(capsys, "api", "user")[:2] == (0, '{"login": "me"}\n')
    code, out, _ = _run(capsys, "label", "list", "--repo", "o/r", "--json", "name")
    assert code == 0
    assert json.loads(out) == [{"name": "ready"}]
    assert _state(world)["issues"]["7"]["comments"] == comments

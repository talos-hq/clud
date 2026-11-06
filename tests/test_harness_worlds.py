"""Unit tests for the /grind harness helpers (#1402): worlds, the
AskUserQuestion answerers (hook and MCP permission-prompt stub), and
`questions_before` / `questions_after`. No Claude Code needed."""

from __future__ import annotations

import io
import json
import sys
from pathlib import Path

import pytest

from tests.harness import answer_hook, answer_mcp, worlds
from tests.harness.harness import ANSWER_TOOL, RunResult, questions_after, questions_before


@pytest.mark.parametrize("name", sorted(worlds.ALL))
def test_world_is_consistent(name: str) -> None:
    state = worlds.ALL[name]()
    issues = state["issues"]
    assert state["repo"] == "o/r"
    assert state["default_branch"] == "main"
    assert len(set(issues)) == len(issues)
    assert all(int(k) < state["next_id"] for k in issues)
    for key, issue in issues.items():
        for sub in issue["sub_issues"]:
            assert issues[str(sub["number"])]["parent"] == int(key)
        if issue["parent"] is not None:
            parent = issues[str(issue["parent"])]
            assert int(key) in [s["number"] for s in parent["sub_issues"]]


def test_grind_sub_meta_is_marked() -> None:
    issue = worlds.with_grind_sub_meta()["issues"]["3"]
    assert "grind:meta" in issue["labels"]
    # The marker the /grind skill's regroup step classifies sub-metas by.
    assert "<!-- grind:v1 -->" in issue["body"]
    user = worlds.with_user_sub_meta()["issues"]["3"]
    assert "grind:meta" not in user["labels"]
    assert "<!-- grind:v1 -->" not in user["body"]


def _children(state: dict, parent: str) -> list[int]:
    return [s["number"] for s in state["issues"][parent]["sub_issues"]]


def test_world_shapes() -> None:
    assert list(worlds.single_change()["issues"]) == ["1"]
    assert "1. " in worlds.multi_part()["issues"]["1"]["body"]
    assert _children(worlds.meta_with_sub_issues(4), "1") == [2, 3, 4, 5]
    task_list = worlds.task_list_meta()
    assert _children(task_list, "1") == []
    assert "- [ ] #2" in task_list["issues"]["1"]["body"]
    listed = worlds.issue_list()
    assert sorted(listed["issues"], key=int) == ["7", "8", "9"]
    assert all(i["parent"] is None for i in listed["issues"].values())
    # Mixed: bugs and feature parts under one meta, holding exactly #1-#5.
    mixed = worlds.mixed()
    assert sorted(mixed["issues"], key=int) == ["1", "2", "3", "4", "5"]
    labels = {n: mixed["issues"][str(n)]["labels"] for n in _children(mixed, "1")}
    assert labels == {2: ["bug"], 3: ["bug"], 4: ["enhancement"], 5: ["enhancement"]}
    # Regroupable: >= 2 groups of >= 3, >= 8 children (#1392 H4).
    kids = _children(worlds.regroupable(), "1")
    assert len(kids) >= 8
    titles = [worlds.regroupable()["issues"][str(n)]["title"] for n in kids]
    groups = {t.split(":", 1)[0] for t in titles}
    assert len(groups) >= 2
    assert all(sum(t.startswith(g) for t in titles) >= 3 for g in groups)
    for build in (worlds.with_user_sub_meta, worlds.with_grind_sub_meta):
        state = build()
        assert _children(state, "1") == [2, 3]
        assert _children(state, "3") == [4, 5]


def _hook(monkeypatch: pytest.MonkeyPatch, tmp_path: Path, event: dict, table: dict) -> int:
    path = tmp_path / "answers.json"
    path.write_text(json.dumps(table), encoding="utf-8")
    monkeypatch.setattr(sys, "argv", ["answer_hook.py", str(path)])
    monkeypatch.setattr(sys, "stdin", io.StringIO(json.dumps(event)))
    return answer_hook.main()


def test_answer_hook_answers_ask_user_question(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    questions = [
        {"question": "Colour?", "header": "c", "options": [{"label": "red"}, {"label": "blue"}]},
        {"question": "Mode?", "header": "Mode", "options": [{"label": "a"}, {"label": "b"}]},
        {"question": "Size?", "header": "s", "options": [{"label": "big"}, {"label": "small"}]},
    ]
    event = {"tool_name": "AskUserQuestion", "tool_input": {"questions": questions}}
    assert _hook(monkeypatch, tmp_path, event, {"Colour?": "blue", "Mode": "b"}) == 0
    out = json.loads(capsys.readouterr().out)["hookSpecificOutput"]
    assert out["hookEventName"] == "PreToolUse"
    assert out["permissionDecision"] == "allow"
    assert out["updatedInput"]["questions"] == questions
    assert out["updatedInput"]["answers"] == {"Colour?": "blue", "Mode?": "b", "Size?": "big"}


def test_answer_hook_default_star(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    q = {"question": "Colour?", "options": [{"label": "red"}, {"label": "blue"}]}
    event = {"tool_name": "AskUserQuestion", "tool_input": {"questions": [q]}}
    _hook(monkeypatch, tmp_path, event, {"*": "blue"})
    out = json.loads(capsys.readouterr().out)
    assert out["hookSpecificOutput"]["updatedInput"]["answers"] == {"Colour?": "blue"}


def test_answer_hook_ignores_other_tools(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    event = {"tool_name": "Bash", "tool_input": {"command": "ls"}}
    assert _hook(monkeypatch, tmp_path, event, {"*": "x"}) == 0
    assert capsys.readouterr().out == ""


def _ask(agent_type: str | None = None) -> dict:
    return {"tool_name": "AskUserQuestion", "agent_type": agent_type}


def test_questions_before() -> None:
    hooks = [
        _ask(),
        {"tool_name": "Bash", "agent_type": None},
        _ask(),
        {"tool_name": "Read", "agent_type": "grind-planner"},
        _ask(),
    ]
    result = RunResult(returncode=0, stdout="", requests=[], hooks=hooks)
    assert len(questions_before(result, "grind-planner")) == 2
    assert len(result.questions_before("grind-planner")) == 2
    assert len(questions_before(result, "grind-lander")) == 3
    empty = RunResult(returncode=0, stdout="", requests=[], hooks=[])
    assert questions_before(empty, "grind-planner") == []


def test_questions_after_and_prework_default() -> None:
    hooks = [
        _ask(),
        {"tool_name": "Bash", "agent_type": "grind-prework"},
        _ask("grind-editor"),
        _ask(),
    ]
    result = RunResult(returncode=0, stdout="", requests=[], hooks=hooks)
    assert len(result.questions_before()) == 1
    assert [q["agent_type"] for q in questions_after(result)] == ["grind-editor", None]
    assert questions_after(result, "grind-lander") == []


def test_answer_hook_multi_select_and_log(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    q = {"question": "Which?", "multiSelect": True, "options": [{"label": "a"}, {"label": "b"}]}
    event = {
        "tool_name": "AskUserQuestion",
        "agent_type": None,
        "tool_input": {"questions": [q]},
    }
    table = tmp_path / "answers.json"
    table.write_text(json.dumps({"Which?": ["a", "b"]}), encoding="utf-8")
    log = tmp_path / "questions.jsonl"
    monkeypatch.setattr(sys, "argv", ["answer_hook.py", str(table), str(log)])
    monkeypatch.setattr(sys, "stdin", io.StringIO(json.dumps(event)))
    assert answer_hook.main() == 0
    out = json.loads(capsys.readouterr().out)
    assert out["hookSpecificOutput"]["updatedInput"]["answers"] == {"Which?": "a, b"}
    record = json.loads(log.read_text(encoding="utf-8"))
    assert (record["question"], record["answer"]) == ("Which?", "a, b")
    assert isinstance(record["t"], int)


def test_answer_mcp_server_answers_ask_and_denies_the_rest(tmp_path: Path) -> None:
    assert ANSWER_TOOL == answer_mcp.TOOL_NAME
    table = {"*": "blue"}
    log = str(tmp_path / "questions.jsonl")
    init = answer_mcp.handle(
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "x"}},
        table,
        log,
    )
    assert init["result"]["protocolVersion"] == "x"
    note = {"jsonrpc": "2.0", "method": "notifications/initialized"}
    assert answer_mcp.handle(note, table, log) is None
    tools = answer_mcp.handle({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}, table, log)
    assert [t["name"] for t in tools["result"]["tools"]] == [answer_mcp.TOOL]
    q = {"question": "Colour?", "options": [{"label": "red"}, {"label": "blue"}]}
    call = {
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": answer_mcp.TOOL,
            "arguments": {"tool_name": "AskUserQuestion", "input": {"questions": [q]}},
        },
    }
    reply = answer_mcp.handle(call, table, log)
    decision = json.loads(reply["result"]["content"][0]["text"])
    assert decision["behavior"] == "allow"
    assert decision["updatedInput"]["answers"] == {"Colour?": "blue"}
    call["params"]["arguments"] = {"tool_name": "Bash", "input": {"command": "ls"}}
    reply = answer_mcp.handle(call, table, log)
    assert json.loads(reply["result"]["content"][0]["text"])["behavior"] == "deny"
    unknown = answer_mcp.handle({"jsonrpc": "2.0", "id": 4, "method": "nope"}, table, log)
    assert unknown["error"]["code"] == -32601

"""Unit coverage for `ci/grind_tool_stats.py` (#1339)."""

from __future__ import annotations

import json
from pathlib import Path

from ci import grind_tool_stats as stats


def _use(i: int, name: str, **args) -> dict:
    return {"type": "tool_use", "id": f"t{i}", "name": name, "input": args}


def _result(i: int, text: str, *, error: bool = False) -> dict:
    return {"type": "tool_result", "tool_use_id": f"t{i}", "content": text, "is_error": error}


def _agent(run: Path, name: str, agent_type: str, turns: list[list[dict]]) -> None:
    (run / f"agent-{name}.meta.json").write_text(json.dumps({"agentType": agent_type}))
    lines = [json.dumps({"message": {"content": blocks}}) for blocks in turns]
    (run / f"agent-{name}.jsonl").write_text("\n".join(lines) + "\nnot json\n")


def test_counts_repeats_within_a_run(tmp_path: Path) -> None:
    run = tmp_path / "wf_1"
    run.mkdir()
    _agent(
        run,
        "a",
        "grind-editor",
        [
            [_use(1, "Read", file_path="/r/a.rs"), _use(2, "Bash", command="gh issue view 7")],
            [_result(1, "x" * 30), _result(2, "body")],
        ],
    )
    _agent(
        run,
        "b",
        "grind-integrator",
        [
            [
                _use(3, "Read", file_path="/r/a.rs", offset=10),
                _use(4, "Bash", command="gh issue view 7 && sleep 30; gh pr checks 5"),
                _use(5, "Edit", file_path="/r/a.rs"),
                _use(6, "Bash", command='"$CLUD_EXE" tool run code/outline.py a.rs'),
            ],
            [_result(3, "y" * 10), _result(5, "no match", error=True)],
        ],
    )
    result = stats.report(stats.collect([run]))
    assert result["runs"] == 1
    assert result["agents_by_role"] == {"worker": 1, "integrator": 1}
    assert result["calls_per_agent_by_role"] == {"worker": 2.0, "integrator": 4.0}
    assert (result["read"], result["read_repeat"], result["read_sliced"]) == (2, 1, 1)
    assert (result["gh_issue_view"], result["gh_issue_view_repeat"]) == (2, 1)
    assert result["bash_wait_loops"] == 1
    assert result["bash_ci_polls"] == 1
    assert result["edit_failed"] == 1
    assert result["uses_code/outline.py"] == 1
    assert result["largest_result_chars"] == 30


def test_repeats_do_not_carry_across_runs(tmp_path: Path) -> None:
    for name in ("wf_1", "wf_2"):
        run = tmp_path / name
        run.mkdir()
        _agent(run, "a", "grind-planner", [[_use(1, "Read", file_path="/r/a.rs")]])
    result = stats.report(stats.collect([tmp_path / "wf_1", tmp_path / "wf_2"]))
    assert (result["runs"], result["read"], result["read_repeat"]) == (2, 2, 0)

"""Real launcher + mock harness JSONL: compact progress and complete final stdout."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from tests import process

pytestmark = pytest.mark.integration


@pytest.mark.parametrize("backend", ["claude", "codex"])
@pytest.mark.parametrize("locale", ["C", "C.UTF-8"])
def test_print_output_classifies_and_preserves_answer(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, backend: str, locale: str
) -> None:
    if backend == "claude":
        events = [
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {"type": "thinking", "thinking": "SECRET_THOUGHT"},
                        {
                            "type": "tool_use",
                            "id": "call1",
                            "name": "Edit",
                            "input": {
                                "file_path": "café.rs",
                                "old_string": "old\nline",
                                "new_string": "new",
                            },
                        },
                    ]
                },
            },
            {
                "type": "user",
                "tool_use_result": {"originalFile": "old\nline"},
                "message": {
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "call1",
                            "content": "NOISY_TOOL_BODY",
                        }
                    ]
                },
            },
            {"type": "result", "is_error": False, "result": "All done café\nFull final answer"},
        ]
    else:
        events = [
            {
                "type": "item.completed",
                "item": {"id": "r", "type": "reasoning", "text": "SECRET_THOUGHT"},
            },
            {
                "type": "item.started",
                "item": {"id": "c", "type": "command_execution", "command": "test command"},
            },
            {
                "type": "item.completed",
                "item": {
                    "id": "c",
                    "type": "command_execution",
                    "command": "test command",
                    "exit_code": 0,
                    "aggregated_output": "NOISY_TOOL_BODY",
                },
            },
            {
                "type": "item.completed",
                "item": {
                    "id": "f",
                    "type": "file_change",
                    "changes": [{"path": "café.rs", "kind": "update"}],
                },
            },
            {
                "type": "item.completed",
                "item": {
                    "id": "m",
                    "type": "agent_message",
                    "text": "All done café\nFull final answer",
                },
            },
            {"type": "turn.completed", "usage": {}},
        ]
    script = tmp_path / "events.jsonl"
    script.write_text("\n".join(json.dumps(e) for e in events), encoding="utf-8")
    warning = tmp_path / "warning.bin"
    warning.write_bytes(b"warning \xff\n")
    env = {**mock_env, "LC_ALL": locale, "TERM": "dumb"}
    result = process.run(
        [
            str(clud_binary),
            f"--{backend}",
            "-p",
            "do work",
            "--",
            "--mock-ansi-script",
            str(warning),
            "--mock-stream-json",
            str(script),
        ],
        env=env,
        cwd=tmp_path,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert ("[message] warning ?" if locale == "C" else "[message] warning \ufffd") in result.stderr
    assert "[thinking] Thinking" in result.stderr
    assert "[edit]" in result.stderr
    assert "[done]" in result.stderr
    assert "SECRET_THOUGHT" not in result.stdout + result.stderr
    assert "NOISY_TOOL_BODY" not in result.stdout + result.stderr
    expected = "All done caf?" if locale == "C" else "All done café"
    assert result.stdout.splitlines() == [expected, "Full final answer"]
    if backend == "claude":
        assert "(+1/-2 lines; 1 output line)" in result.stderr
    else:
        assert result.stderr.count("test command") == 1
        assert "Shell: 1 output line" in result.stderr


@pytest.mark.parametrize("backend", ["claude", "codex"])
def test_mock_edits_report_exact_changed_lines(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, backend: str
) -> None:
    """The mock really writes files. Context, CRLF, blank lines and EOF must not inflate counts."""
    import difflib

    cases = [
        ("context.txt", "same\nold\nkeep\nremove\n", "same\nnew\nextra\nkeep\n", 2, 2),
        ("insert.txt", "a\nb\n", "a\n\nx\nb\n", 2, 0),
        ("delete.txt", "a\nb\nc", "a\n", 0, 2),
        ("crlf.txt", "same\r\nold\r\nend\r\n", "same\r\nnew\r\nend\r\n", 1, 1),
        ("unchanged.txt", "same\n", "same\n", 0, 0),
        ("replace-all.txt", "x x\n", "y y\n", 1, 1),
        ("0/a.rs", "", "one\n", 1, 0),
        ("a.rs", "", "one\ntwo\nthree\nfour\nfive\n", 5, 0),
    ]
    before_events, after_events, edits, records = [], [], [], []
    thread = "00000000-1111-2222-3333-444444444444"
    if backend == "codex":
        before_events.append({"type": "thread.started", "thread_id": thread})
        records.append({"type": "event_msg", "payload": {"type": "task_started"}})
    for index, (name, old, new, added, removed) in enumerate(cases):
        target = tmp_path / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(old.encode())
        edits.append({"path": str(target), "content": new})
        patch = "".join(
            difflib.unified_diff(
                old.splitlines(True), new.splitlines(True), "a/" + name, "b/" + name
            )
        )
        if backend == "claude":
            before_events.append(
                {
                    "type": "assistant",
                    "message": {
                        "content": [
                            {
                                "type": "tool_use",
                                "id": f"edit{index}",
                                "name": "Edit",
                                "input": {
                                    "file_path": name,
                                    "old_string": "x" if name == "replace-all.txt" else old,
                                    "new_string": "y" if name == "replace-all.txt" else new,
                                    "replace_all": name == "replace-all.txt",
                                },
                            }
                        ]
                    },
                }
            )
            after_events.append(
                {
                    "type": "user",
                    "tool_use_result": {"originalFile": old},
                    "message": {
                        "content": [
                            {
                                "type": "tool_result",
                                "tool_use_id": f"edit{index}",
                                "content": "Edited\nOK\n",
                            }
                        ]
                    },
                }
            )
        else:
            records.append(
                {
                    "type": "event_msg",
                    "payload": {
                        "type": "patch_apply_end",
                        "call_id": f"call{index}",
                        "success": True,
                        "changes": {
                            str(target): {
                                "type": "update",
                                "unified_diff": patch,
                                "move_path": None,
                            }
                        },
                    },
                }
            )
            after_events.append(
                {
                    "type": "item.completed",
                    "item": {
                        "id": f"item{index}",
                        "type": "file_change",
                        "status": "completed",
                        "changes": [{"path": name, "kind": "update"}],
                    },
                }
            )
        # These constants are independently checked against difflib's before/after diff.
        delta = list(difflib.ndiff(old.splitlines(), new.splitlines()))
        assert sum(line.startswith("+ ") for line in delta) == added
        assert sum(line.startswith("- ") for line in delta) == removed
    if backend == "codex":
        # Same call, duplicate basenames: match complete paths one-to-one.
        changes = {
            path: change
            for record in records[1:]
            for path, change in record["payload"]["changes"].items()
        }
        records = [
            *records[:1],
            {
                "type": "event_msg",
                "payload": {
                    "type": "patch_apply_end",
                    "call_id": "all-edits",
                    "success": True,
                    "changes": changes,
                },
            },
        ]
        paths = [change for event in after_events for change in event["item"]["changes"]]
        after_events = [
            {
                "type": "item.completed",
                "item": {
                    "id": "all-edits",
                    "type": "file_change",
                    "status": "completed",
                    "changes": paths,
                },
            }
        ]
    if backend == "claude":
        after_events.append({"type": "result", "is_error": False, "result": "Edits complete"})
    else:
        after_events.extend(
            [
                {
                    "type": "item.completed",
                    "item": {"id": "final", "type": "agent_message", "text": "Edits complete"},
                },
                {"type": "turn.completed", "usage": {}},
            ]
        )
    codex_home = tmp_path / "codex-home"
    scenario = tmp_path / "scenario.json"
    scenario.write_text(
        json.dumps(
            {
                "before_events": before_events,
                "edits": edits,
                "after_events": after_events,
                "session_log": str(
                    codex_home / "sessions" / "2026" / "11" / "03" / f"rollout-test-{thread}.jsonl"
                ),
                "session_records": records,
            }
        ),
        encoding="utf-8",
    )
    result = process.run(
        [str(clud_binary), f"--{backend}", "-p", "edit files"],
        env={
            **mock_env,
            "MOCK_AGENT_PROMPT_TOOLS": str(scenario),
            "CODEX_HOME": str(codex_home),
            "LC_ALL": "C",
            "TERM": "dumb",
        },
        cwd=tmp_path,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert result.stdout.splitlines() == ["Edits complete"]
    for name, _old, new, added, removed in cases:
        assert (tmp_path / name).read_bytes() == new.encode()
        if backend == "claude":
            assert f"{name} (+{added}/-{removed} lines; 2 output lines)" in result.stderr, (
                result.stderr
            )
    if backend == "codex":
        added = sum(case[3] for case in cases)
        removed = sum(case[4] for case in cases)
        assert f"{len(cases)} files (+{added}/-{removed} lines)" in result.stderr, result.stderr
        edit_lines = [line for line in result.stderr.splitlines() if line.startswith("[edit]")]
        assert len(edit_lines) == 1
        assert len(edit_lines[0]) < 180
    assert "line counts unavailable" not in result.stderr


@pytest.mark.parametrize("backend", ["claude", "codex"])
def test_explicit_native_json_output_is_not_summarized(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, backend: str
) -> None:
    event = {"type": "error", "message": "native output", "error": {"message": "native output"}}
    script = tmp_path / "raw.jsonl"
    script.write_text(json.dumps(event) + "\n", encoding="utf-8")
    flags = ["--output-format", "stream-json"] if backend == "claude" else ["--json"]
    result = process.run(
        [
            str(clud_binary),
            f"--{backend}",
            "-p",
            "hello",
            "--",
            *flags,
            "--mock-stream-json",
            str(script),
            "--mock-exit-code",
            "7",
        ],
        env=mock_env,
        cwd=tmp_path,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 7
    assert json.loads(result.stdout) == event
    assert "[error]" not in result.stderr


@pytest.mark.parametrize("backend", ["claude", "codex"])
def test_summarized_failures_keep_exit_code_and_cause(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, backend: str
) -> None:
    event = {"type": "error", "message": "quota exhausted", "error": {"message": "quota exhausted"}}
    script = tmp_path / "error.jsonl"
    script.write_text(json.dumps(event) + "\n", encoding="utf-8")
    result = process.run(
        [
            str(clud_binary),
            f"--{backend}",
            "-p",
            "hello",
            "--",
            "--mock-stream-json",
            str(script),
            "--mock-exit-code",
            "7",
        ],
        env=mock_env,
        cwd=tmp_path,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 7
    assert "[error] quota exhausted" in result.stderr
    assert result.stdout == ""


@pytest.mark.parametrize("backend", ["claude", "codex"])
def test_each_tool_result_counts_text_blocks_separately(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path, backend: str
) -> None:
    blocks = [{"type": "text", "text": "one"}, {"type": "text", "text": "two\n\nthree\n"}]
    if backend == "claude":
        events = [
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "read",
                            "name": "Read",
                            "input": {"file_path": "a.rs"},
                        }
                    ]
                },
            },
            {
                "type": "user",
                "message": {
                    "content": [{"type": "tool_result", "tool_use_id": "read", "content": blocks}]
                },
            },
            {"type": "result", "is_error": False, "result": "done"},
        ]
    else:
        events = [
            {
                "type": "item.completed",
                "item": {
                    "id": "mcp",
                    "type": "mcp_tool_call",
                    "server": "files",
                    "tool": "read",
                    "status": "completed",
                    "result": {"content": blocks},
                },
            },
            {"type": "turn.completed", "usage": {}},
        ]
    script = tmp_path / "blocks.jsonl"
    script.write_text("\n".join(json.dumps(e) for e in events), encoding="utf-8")
    result = process.run(
        [str(clud_binary), f"--{backend}", "-p", "hello", "--", "--mock-stream-json", str(script)],
        env=mock_env,
        cwd=tmp_path,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stderr
    name = "Read" if backend == "claude" else "files/read"
    assert f"{name}: 4 output lines" in result.stderr

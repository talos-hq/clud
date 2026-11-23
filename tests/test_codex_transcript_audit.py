"""Bounded, privacy-preserving cross-turn tool-sequence audit (#2232)."""

import importlib.util
import json
from pathlib import Path

import pytest

TOOL = (
    Path(__file__).resolve().parents[1] / "crates/clud-bin/assets/tools/diagnostics/codex_audit.py"
)


@pytest.fixture
def audit():
    spec = importlib.util.spec_from_file_location("codex_audit", TOOL)
    assert spec
    assert spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def codex(kind, **values):
    return {"type": "response_item", "payload": {"type": kind, **values}}


def test_codex_deduplicates_results_and_distinguishes_waits(audit):
    rows = [
        codex(
            "custom_tool_call",
            call_id="c1",
            name="exec",
            input='tools.exec_command({cmd:"private-secret"})',
        ),
        codex(
            "custom_tool_call_output",
            call_id="c1",
            id="output-one",
            output=[{"type": "input_text", "text": "session_id: 77"}],
        ),
        codex(
            "custom_tool_call_output",
            call_id="c1",
            id="output-one",
            output=[{"type": "input_text", "text": "session_id: 77"}],
        ),
        {"type": "turn_context", "payload": {}},
        codex(
            "custom_tool_call",
            call_id="c2",
            name="exec",
            input='tools.write_stdin({session_id:77,chars:""})',
        ),
        codex(
            "custom_tool_call_output",
            call_id="c2",
            output=[{"type": "input_text", "text": "exit_code: 0"}],
        ),
    ]
    result = audit.analyze(rows, salt=b"x" * 32)
    assert result["tool_calls"] == 2
    assert result["tool_results"] == 2
    assert result["duplicate_results"] == 1
    assert result["output_bytes"] == len("session_id: 77") + len("exit_code: 0")
    assert result["categories"]["wait"] == 1
    assert result["nested_tool_mentions"]["exec_command"] == 1
    assert result["nested_tool_mentions"]["write_stdin"] == 1
    assert result["wait_observations"]["terminal"] == 1
    assert result["turns"] == 1
    assert "private-secret" not in json.dumps(result)
    assert '"77"' not in json.dumps(result)


def test_claude_calls_and_results_are_counted_without_output_content(audit):
    rows = [
        {
            "type": "assistant",
            "message": {
                "id": "message-one",
                "content": [
                    {
                        "type": "tool_use",
                        "id": "call-one",
                        "name": "Bash",
                        "input": {"command": "secret_command"},
                    }
                ],
            },
        },
        {
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "call-one",
                        "content": "error: private_path",
                    }
                ]
            },
        },
    ]
    result = audit.analyze(rows, salt=b"x" * 32)
    assert result["tool_calls"] == 1
    assert result["tool_results"] == 1
    assert result["output_classes"]["failure"] == 1
    serialized = json.dumps(result)
    assert "secret_command" not in serialized
    assert "private_path" not in serialized
    assert "call-one" not in serialized


def test_changed_read_outputs_are_distinguished_without_claiming_waste(audit):
    rows = []
    for i, value in enumerate(("first", "first", "modified")):
        rows.extend(
            [
                codex(
                    "function_call",
                    call_id=f"c{i}",
                    name="exec_command",
                    arguments='{"cmd":"cat secret-file","workdir":"/tmp/work"}',
                ),
                codex("function_call_output", call_id=f"c{i}", id=f"output-{i}", output=value),
            ]
        )
    result = audit.analyze(rows, salt=b"x" * 32)
    assert result["repeated_calls"] == 2
    assert result["repeated_by_category"]["read"] == 2
    assert result["repeated_simple_command_shapes"] == 2
    assert result["repeated_simple_read_targets"] == 2
    assert result["repeated_read_observations"]["same_observed_text"] == 1
    assert result["repeated_read_observations"]["changed_observed_text"] == 1
    assert "secret-file" not in json.dumps(result)


def test_wait_markers_are_observations_not_waste_verdicts(audit):
    rows = []
    for i, output in enumerate(
        ("session_id: 42", "exit_code: 0", "session not found", "no status")
    ):
        rows.extend(
            [
                codex("function_call", call_id=f"wait-{i}", name="write_stdin", arguments="{}"),
                codex("function_call_output", call_id=f"wait-{i}", output=output),
            ]
        )
    result = audit.analyze(rows, salt=b"s" * 32)
    assert result["wait_observations"] == {"live": 1, "terminal": 1, "missing": 1, "unknown": 1}
    assert "unverified" in result["wait_interpretation"]


def test_binary_and_malformed_content_are_not_counted_as_tokens(audit, tmp_path):
    import io

    rows = [
        codex("custom_tool_call", call_id="secret-id", name="exec", input="ignored"),
        codex(
            "custom_tool_call_output",
            call_id="secret-id",
            output=[
                {"type": "input_text", "text": "useful"},
                {"type": "input_image", "image_url": "data:image/png;base64," + "a" * 400},
            ],
        ),
        None,
    ]
    report = audit.analyze(rows, salt=b"x" * 32)
    assert report["output_bytes"] == len("useful")
    assert report["incomplete_results"] == 1
    assert report["malformed_rows"] == 1
    assert "not model tokens" in report["byte_scope"]
    assert "a" * 100 not in json.dumps(report)
    assert "secret-id" not in json.dumps(report)
    del io, tmp_path


def test_reader_reports_truncated_and_invalid_rows(audit, monkeypatch, tmp_path):
    from collections import Counter

    monkeypatch.setattr(audit, "MAX_ROW_BYTES", 32)
    file = tmp_path / "sample.jsonl"
    file.write_bytes(b"{bad json}\n" + b"x" * 100 + b"\n" + b'{"type":"turn_context"}\n')
    stats = Counter(oversized_rows=0, malformed_json_rows=0)
    report = audit.analyze(audit.read_rows(file, stats), salt=b"x" * 32)
    assert stats["oversized_rows"] == 1
    assert stats["malformed_json_rows"] == 1
    assert report["turns"] == 1


def test_conflicting_duplicate_records_are_explicit(audit):
    rows = [
        codex("function_call", call_id="same", name="exec_command", arguments='{"cmd":"cat file"}'),
        codex(
            "function_call", call_id="same", name="exec_command", arguments='{"cmd":"cat changed"}'
        ),
        codex("function_call_output", call_id="same", id="output-same", output="first"),
        codex("function_call_output", call_id="same", id="output-same", output="changed"),
        codex("function_call_output", call_id="orphan", output="orphan-result"),
    ]
    result = audit.analyze(rows, salt=b"a" * 32)
    assert result["tool_calls"] == 1
    assert result["tool_results"] == 2
    assert result["duplicate_calls"] == 1
    assert result["conflicting_duplicate_calls"] == 1
    assert result["duplicate_results"] == 1
    assert result["conflicting_or_unknown_duplicate_results"] == 1
    assert result["unmatched_results"] == 1
    assert result["output_bytes"] == len("first") + len("orphan-result")


def test_nested_batched_literal_commands_are_counted_without_targets(audit):
    source = (
        'await tools.exec_command({cmd:"cat /private/A"});'
        'await tools.exec_command({cmd:"cat /private/B"});'
        'await tools.exec_command({cmd:"cat /private/A"});'
    )
    rows = [codex("custom_tool_call", call_id="batched", name="exec", input=source)]
    result = audit.analyze(rows, salt=b"y" * 32)
    assert result["nested_tool_mentions"]["exec_command"] == 3
    assert result["repeated_simple_command_shapes"] == 2
    assert result["repeated_simple_read_targets"] == 1
    assert "not necessarily executed" in result["nested_interpretation"]
    assert "/private/A" not in json.dumps(result)


def test_distinct_output_chunks_of_one_call_are_all_counted(audit):
    rows = [
        codex("custom_tool_call", call_id="call", name="exec", input="script"),
        codex("custom_tool_call_output", call_id="call", id="chunk-1", output="one"),
        codex("custom_tool_call_output", call_id="call", id="chunk-2", output="two"),
        codex("custom_tool_call_output", call_id="call", output="three"),
    ]
    result = audit.analyze(rows, salt=b"s" * 32)
    assert result["tool_results"] == 3
    assert result["output_bytes"] == len("onetwothree")
    assert result["duplicate_results"] == 0
    assert result["result_identity_unknown"] == 1


def test_nested_tracking_is_bounded(audit, monkeypatch):
    monkeypatch.setattr(audit, "MAX_TRACKED", 2)
    monkeypatch.setattr(audit, "MAX_NESTED_MENTIONS", 2)
    source = "".join(f'tools.exec_command({{cmd:"cat file-{i}"}});' for i in range(10))
    result = audit.analyze(
        [codex("custom_tool_call", call_id="batch", name="exec", input=source)], salt=b"s" * 32
    )
    assert result["nested_tool_mentions"]["exec_command"] == 2
    assert result["nested_mentions_truncated"] is True


def test_invalid_arguments_do_not_echo_secrets(audit, capsys):
    assert audit.main(["--private-secret", "/sensitive/path"]) == 2
    output = capsys.readouterr()
    assert "private-secret" not in output.err
    assert "/sensitive/path" not in output.err


def test_repeated_reads_compare_all_chunks_as_one_observation(audit):
    rows = []
    for i in range(2):
        rows.extend(
            [
                codex(
                    "function_call",
                    call_id=f"call-{i}",
                    name="exec_command",
                    arguments='{"cmd":"cat secret"}',
                ),
                codex("function_call_output", call_id=f"call-{i}", id=f"{i}-first", output="first"),
                codex(
                    "function_call_output", call_id=f"call-{i}", id=f"{i}-second", output="second"
                ),
            ]
        )
    report = audit.analyze(rows, salt=b"x" * 32)
    assert report["tool_results"] == 4
    assert report["repeated_read_observations"] == {
        "same_observed_text": 1,
        "changed_observed_text": 0,
        "unknown": 0,
    }


def test_interrupted_reads_only_claim_observed_prefix_equality(audit):
    rows = []
    for i in range(2):
        rows.extend(
            [
                codex(
                    "function_call",
                    call_id=f"partial-{i}",
                    name="exec_command",
                    arguments='{"cmd":"cat secret"}',
                ),
                codex(
                    "function_call_output",
                    call_id=f"partial-{i}",
                    id=f"event-{i}",
                    output="shared prefix",
                ),
            ]
        )
    report = audit.analyze(rows, salt=b"x" * 32)
    assert report["repeated_read_observations"]["same_observed_text"] == 1
    assert "unknown" in report["read_completeness"]
    assert "does not establish unchanged input" in report["read_completeness"]


def test_relative_target_without_known_cwd_is_not_claimed_identical(audit):
    rows = [
        codex(
            "function_call", call_id=str(i), name="exec_command", arguments='{"cmd":"cat relative"}'
        )
        for i in range(2)
    ]
    report = audit.analyze(rows, salt=b"x" * 32)
    assert report["repeated_simple_read_targets"] == 0
    assert "identity unverified" in report["target_interpretation"]


def test_deeply_nested_row_is_malformed_not_a_traceback(audit, tmp_path, monkeypatch):
    from collections import Counter

    path = tmp_path / "deep.jsonl"
    path.write_text("[" * 2000 + "]" * 2000 + "\n" + '{"type":"turn_context"}\n')
    stats = Counter(oversized_rows=0, malformed_json_rows=0)
    original_loads = audit.json.loads

    def bounded_decoder(data, *args, **kwargs):
        if isinstance(data, bytes) and data.startswith(b"["):
            raise RecursionError("test decoder depth")
        return original_loads(data, *args, **kwargs)

    monkeypatch.setattr(audit.json, "loads", bounded_decoder)
    report = audit.analyze(audit.read_rows(path, stats), salt=b"x" * 32)
    assert stats["malformed_json_rows"] == 1
    assert report["turns"] == 1


def test_timestamped_compiler_failure_and_pass_chatter_are_classified(audit):
    output = "5.32 \x1b[31merror[E0308]: mismatch\x1b[0m\n5.33 test sample ... ok\n"
    rows = [
        codex("function_call", call_id="call", name="exec_command", arguments="{}"),
        codex("function_call_output", call_id="call", id="output", output=output),
    ]
    report = audit.analyze(rows, salt=b"x" * 32)
    assert report["output_classes"]["failure"] == 1
    assert report["output_classes"]["progress"] == 1
    assert "mismatch" not in json.dumps(report)


def test_timeout_and_cancellation_markers_are_preserved(audit):
    rows = []
    for i, output in enumerate(("command timed out", "run cancelled", "configured timeout=30")):
        rows.extend(
            [
                codex("function_call", call_id=f"c{i}", name="exec_command", arguments="{}"),
                codex("function_call_output", call_id=f"c{i}", id=f"out-{i}", output=output),
            ]
        )
    report = audit.analyze(rows, salt=b"x" * 32)
    assert report["output_classes"]["timeout"] == 1
    assert report["output_classes"]["cancellation"] == 1


def test_github_command_markers_are_classified(audit):
    rows = [
        codex("function_call", call_id="c", name="exec_command", arguments="{}"),
        codex(
            "function_call_output",
            call_id="c",
            id="out",
            output="job step ##[error]Process failed\n##[warning]notice",
        ),
    ]
    report = audit.analyze(rows, salt=b"x" * 32)
    assert report["output_classes"]["failure"] == 1
    assert report["output_classes"]["warning"] == 1


def test_repeated_call_pairs_cross_turns_without_disclosing_arguments(audit):
    rows = [
        codex("custom_tool_call", call_id="a1", name="Bash", input="cat /private/a"),
        codex("custom_tool_call", call_id="b1", name="Bash", input="git status"),
        {"type": "turn_context", "payload": {}},
        codex("custom_tool_call", call_id="a2", name="Bash", input="cat /private/a"),
        codex("custom_tool_call", call_id="b2", name="Bash", input="git status"),
    ]
    result = audit.analyze(rows, salt=b"x" * 32)
    assert result["repeated_call_pairs"] == 1
    assert result["repeated_pairs_by_category"]["state_probe"] == 1
    assert "/private/a" not in json.dumps(result)


def test_result_id_reused_for_different_calls_is_uncertain(audit):
    rows = [
        codex("custom_tool_call", call_id="a", name="Bash", input="cat /private/a"),
        codex("custom_tool_call_output", call_id="a", id="same", output="same text"),
        codex("custom_tool_call", call_id="b", name="Bash", input="cat /private/a"),
        codex("custom_tool_call_output", call_id="b", id="same", output="same text"),
    ]
    result = audit.analyze(rows, salt=b"x" * 32)
    assert result["conflicting_or_unknown_duplicate_results"] == 1
    assert result["repeated_read_observations"]["unknown"] == 1
    assert "/private/a" not in json.dumps(result)


def test_conflicting_unknown_call_ids_do_not_expand_uncertainty_tracking(audit):
    result = {
        "duplicate_results": 0,
        "conflicting_or_unknown_duplicate_results": 0,
        "tracking_truncated": False,
        "result_identity_unknown": 0,
    }
    seen = {}
    uncertain = set()
    salt = b"x" * 32
    known = {b"known": (b"shape", "read")}
    audit.is_replayed_result(
        "event", b"known", b"text", False, salt, result, seen, uncertain, known
    )
    for index in range(100):
        audit.is_replayed_result(
            "event", str(index).encode(), b"text", False, salt, result, seen, uncertain, known
        )
    assert uncertain == {b"known"}


def test_cohort_preserves_file_identity_and_counts_friction(audit, tmp_path, capsys):
    codex_path = tmp_path / "private-codex.jsonl"
    claude_path = tmp_path / "private-claude.jsonl"
    refusal = "Command blocked by PreToolUse hook: private-content"
    capped = "Warning: truncated output\n" + "x" * 32768
    first_result = codex("function_call_output", call_id="private-id", id="shared", output=refusal)
    codex_rows = [
        codex("function_call", call_id="private-id", name="wait", arguments="{}"),
        first_result,
        first_result,
        codex("function_call", call_id="followup", name="wait", arguments="{}"),
        codex("function_call_output", call_id="followup", id="other", output=capped),
        {"type": "compacted"},
    ]
    claude_rows = [
        {
            "type": "assistant",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "private-id",
                        "name": "Bash",
                        "input": {"command": "cat file"},
                    }
                ]
            },
        },
        {
            "type": "user",
            "uuid": "claude-result",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "private-id",
                        "content": "PreToolUse hook: [hand-written-wait] Refused a private-command",
                    }
                ]
            },
        },
    ]
    for path, rows in ((codex_path, codex_rows), (claude_path, claude_rows)):
        path.write_text("".join(json.dumps(row) + "\n" for row in rows))

    assert audit.main([str(codex_path), str(claude_path)]) == 0
    output = capsys.readouterr().out
    report = json.loads(output)
    assert report["transcripts"] == 2
    assert report["tool_calls"] == report["tool_results"] == 3
    assert report["duplicate_results"] == 1
    assert report["repeated_by_category"]["wait"] == 1
    assert report["compactions"] == 1
    assert report["largest_output_bytes"] == len(capped)
    assert report["friction_results"] == {"hook_refusal": 2, "output_cap": 1, "over_32_kib": 1}
    assert report["transcripts_with_friction"] == {
        "hook_refusal": 2,
        "output_cap": 1,
        "over_32_kib": 1,
    }
    assert "private-" not in output

    assert audit.main([str(codex_path), str(tmp_path / "private-missing")]) == 1
    failure = capsys.readouterr()
    assert not failure.out
    assert failure.err == "codex_audit: could not read transcript\n"

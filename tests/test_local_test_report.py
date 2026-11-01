"""Agent-facing local test reports retain failures without success chatter (#2230)."""

import importlib.util
import io
from pathlib import Path

import pytest

# Tier 1 (#2356): the test_report tool under test spawns the built clud.
pytestmark = pytest.mark.daemon

TOOL = (
    Path(__file__).resolve().parents[1] / "crates/clud-bin/assets/tools/diagnostics/test_report.py"
)


@pytest.fixture
def report():
    spec = importlib.util.spec_from_file_location("local_test_report", TOOL)
    assert spec
    assert spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_many_unittest_subtest_failures_are_grouped_with_traceback(report):
    failures = "".join(
        f"FAIL: test_example (sample.Sample.test_example) (case={i})\n" for i in range(2000)
    )
    raw = (
        failures
        + 'Traceback (most recent call last):\n  File "test_sample.py", line 42\n'
        + "AssertionError: wrong value\nFAILED (failures=2000)\n"
    )
    summary = report.summarize(io.BytesIO(raw.encode()), exit_code=1)
    assert len(summary.encode()) <= 8192
    assert "test_example" in summary
    assert "2000" in summary
    assert "AssertionError: wrong value" in summary
    assert "test_sample.py" in summary
    assert len(summary.encode()) * 10 < len(raw.encode())


def test_pass_chatter_is_suppressed_but_warnings_and_failures_are_not(report):
    raw = "".join(f"test example_{i} ... ok\n" for i in range(5000))
    raw += "warning: deprecated interface\nerror[E0308]: mismatched types\n  --> src/lib.rs:9:5\n"
    summary = report.summarize(io.BytesIO(raw.encode()), exit_code=0)
    assert len(summary.encode()) <= 8192
    assert "example_1" not in summary
    assert "deprecated interface" in summary
    assert "mismatched types" in summary
    assert "anomal" in summary.lower()


PYTEST_WARNING = "test_example.py:9: UserWarning: legacy knob is deprecated\n"


def test_pytest_source_prefixed_warning_kept_beside_failure(report):
    raw = "".join(f"tests/test_x.py::test_{i} PASSED\n" for i in range(100))
    raw += "FAILED tests/test_x.py::test_bad - AssertionError: boom\n"
    raw += "=== warnings summary ===\n" + PYTEST_WARNING
    raw += "=== 1 failed, 100 passed, 1 warning in 0.12s ===\n"
    summary = report.summarize(io.BytesIO(raw.encode()), exit_code=1)
    data = report.json.loads(summary)
    assert data["verdict"] == "failed"
    assert data["exit_code"] == 1
    assert "legacy knob is deprecated" in data["warnings"][0]
    assert {"name": "tests/test_x.py::test_bad", "count": 1} in data["failure_groups"]
    assert "PASSED" not in summary


def test_pytest_warning_only_success_passes_and_keeps_warning(report):
    raw = "".join(f"tests/test_x.py::test_{i} PASSED\n" for i in range(3))
    raw += "=== warnings summary ===\n" + PYTEST_WARNING
    raw += "=== 3 passed, 1 warning in 0.01s ===\n"
    data = report.json.loads(report.summarize(io.BytesIO(raw.encode()), exit_code=0))
    assert data["verdict"] == "passed"
    assert any("legacy knob is deprecated" in w for w in data["warnings"])
    assert data["diagnostics"] == []


def test_pytest_warning_categories_indent_and_unicode_paths(report):
    raw = (
        "  /tmp/pkg/mod.py:12: RuntimeWarning: coroutine never awaited\n"
        "src/ünïcode/test_ü.py:3: FutureWarning: changed\n"
    )
    data = report.json.loads(report.summarize(io.BytesIO(raw.encode()), exit_code=1))
    assert any("coroutine never awaited" in w for w in data["warnings"])
    unicode_entry = "src/ünïcode/test_ü.py:3: FutureWarning"
    assert any(unicode_entry in w for w in data["warnings"])


def test_long_pytest_warning_is_truncated_within_limit(report):
    raw = "tests/test_long.py:7: UserWarning: " + "x" * 600 + "\n"
    summary = report.summarize(io.BytesIO(raw.encode()), exit_code=1, limit=512)
    data = report.json.loads(summary)
    assert len(summary.encode()) + 1 <= 512
    assert data["warnings"][0].startswith("tests/test_long.py:7: UserWarning")
    assert data["input_truncated"]


def test_pytest_warning_cap_counts_omitted_accurately(report):
    total = report.MAX_DIAGNOSTICS + 5
    raw = "".join(f"tests/t.py:{i}: UserWarning: w{i}\n" for i in range(total))
    data = report.json.loads(report.summarize(io.BytesIO(raw.encode()), exit_code=1))
    assert len(data["warnings"]) + data["omitted"]["warnings"] == total


def test_pytest_warning_count_in_summary_is_not_a_warning(report):
    raw = b"=== 5 passed, 2 warnings in 0.1s ===\n"
    data = report.json.loads(report.summarize(io.BytesIO(raw), exit_code=0))
    assert data["warnings"] == []


def test_unfamiliar_output_is_explicitly_unknown(report):
    summary = report.summarize(io.BytesIO(b"strange non-test output\n"), exit_code=1)
    data = report.json.loads(summary)
    assert data["verdict"] == "failed"
    assert data["diagnostic_state"] == "unknown"


@pytest.mark.skipif(
    __import__("sys").platform in {"win32", "darwin"}, reason="ACL checks pending"
)
def test_command_failure_keeps_exact_log_and_exit(report, monkeypatch, tmp_path, capsys):
    import json
    import shlex
    import sys

    monkeypatch.setenv("CLUD_TOOL_OUTPUT_DIR", str(tmp_path))
    command = [
        sys.executable,
        "-c",
        "import sys; sys.stdout.buffer.write(b'error: bad\\r\\n\\xff'); sys.exit(7)",
    ]
    assert report.main(["--", *command]) == 7
    out = capsys.readouterr().out.splitlines()
    result = json.loads(out[0])
    assert result["verdict"] == "failed"
    assert "error: bad" in result["diagnostics"][0]
    artifact = Path(shlex.split(out[1].removeprefix("artifact: "))[0])
    assert artifact.read_bytes() == b"error: bad\r\n\xff"
    assert report.main(["--artifact", str(artifact), "--offset", "12", "--count", "1"]) == 0
    page = json.loads(capsys.readouterr().out)
    assert page["hex"] == "ff"


@pytest.mark.skipif(
    __import__("sys").platform in {"win32", "darwin"}, reason="ACL checks pending"
)
def test_real_timeout_reports_incomplete_and_unverified_cleanup(
    report, monkeypatch, tmp_path, capsys
):
    import json
    import sys

    monkeypatch.setenv("CLUD_TOOL_OUTPUT_DIR", str(tmp_path))
    status = report.main(
        [
            "--limit",
            "512",
            "--timeout",
            "1",
            "--",
            sys.executable,
            "-c",
            "import time; time.sleep(10)",
        ]
    )
    data = json.loads(capsys.readouterr().out.splitlines()[0])
    assert status == 124
    assert data["verdict"] == "incomplete"
    assert data["capture_state"] == "timeout"
    assert data["tree_cleanup"] == "unverified"


@pytest.mark.skipif(
    __import__("sys").platform in {"win32", "darwin"}, reason="ACL checks pending"
)
def test_artifact_failure_prevents_command_execution(report, monkeypatch, capsys):
    calls = []
    monkeypatch.setattr(
        report, "artifact_root", lambda: (_ for _ in ()).throw(OSError("disk full"))
    )
    monkeypatch.setattr(report, "run_command", lambda *_args: calls.append(True))
    assert report.main(["--", "irrelevant"]) != 0
    assert not calls
    assert "disk full" not in capsys.readouterr().err


def test_pytest_rust_and_oversized_unicode_lines(report):
    raw = "FAILED tests/test_file.py::test_example - AssertionError: bad\n"
    raw += "test sample::thing ... FAILED\nerror[E0308]: " + "å" * 100000 + "\n"
    summary = report.summarize(io.BytesIO(raw.encode()), exit_code=1, limit=4096)
    data = report.json.loads(summary)
    assert len(summary.encode()) <= 4096
    assert any("test_example" in item["name"] for item in data["failure_groups"])
    assert any("sample::thing" in item["name"] for item in data["failure_groups"])
    assert any("truncated" in item for item in data["diagnostics"])


def test_thousands_of_passing_tests_emit_concise_success(report):
    raw = "".join(f"test example_{i} ... ok\n" for i in range(5000))
    raw += "warning: material diagnostic\ntest result: ok. 5000 passed; 0 failed\n"
    result = report.json.loads(report.summarize(io.BytesIO(raw.encode()), exit_code=0))
    assert result["verdict"] == "passed"
    assert result["passed_lines"] == 5000
    assert "warning: material diagnostic" in result["warnings"]
    assert len(report.json.dumps(result).encode()) < 2000


def test_tight_budget_never_stalls_on_truncation(report):
    raw = ("warning: " + "x" * 200 + "\n" + "error: " + "x" * 200 + "\n").encode()
    summary = report.summarize(io.BytesIO(raw), exit_code=1, limit=512)
    assert len(summary.encode()) <= 512
    assert "truncated" in summary


def test_soldr_timestamped_diagnostics_are_actionable(report):
    raw = (
        b"    5.32 \x1b[31merror[E0308]: mismatched types\x1b[0m\n    5.32   --> rust/lib.rs:42:9\n"
    )
    result = report.json.loads(report.summarize(io.BytesIO(raw), exit_code=1))
    assert any("mismatched types" in x for x in result["diagnostics"])
    assert any("rust/lib.rs:42:9" in x for x in result["diagnostics"])


def test_unicode_failure_name_fits_small_budget(report):
    raw = ("FAIL: " + "😀" * 300 + "\n").encode()
    output = report.summarize(io.BytesIO(raw), exit_code=1, limit=512)
    assert len(output.encode()) <= 512
    assert "truncated" in output


def test_passing_test_with_panic_is_not_claimed_failed(report):
    raw = (
        b"thread 'tests::expected' panicked at src/lib.rs:3\n"
        b"test tests::expected ... ok\ntest result: ok. 1 passed\n"
    )
    result = report.json.loads(report.summarize(io.BytesIO(raw), exit_code=0))
    assert result["verdict"] == "unknown"
    assert any("panicked" in line for line in result["diagnostics"])


def test_verbose_pytest_and_go_failures_keep_test_identity(report):
    raw = b"tests/test_file.py::test_bad FAILED [ 10%]\n--- FAIL: TestBroken (0.00s)\n"
    result = report.json.loads(report.summarize(io.BytesIO(raw), exit_code=1))
    names = [item["name"] for item in result["failure_groups"]]
    assert "tests/test_file.py::test_bad" in names
    assert "TestBroken" in names


def test_escape_sequences_and_unicode_controls_do_not_reach_terminal(report):
    raw = b"\x1b[31merror: hidden\x1b[0m\n" + "warning: \u202econfusing\n".encode()
    summary = report.summarize(io.BytesIO(raw), exit_code=1)
    assert "\x1b" not in summary
    assert "\u202e" not in summary
    assert "error: hidden" in summary


@pytest.mark.skipif(
    __import__("sys").platform in {"win32", "darwin"}, reason="ACL checks pending"
)
def test_timeout_metadata_is_never_a_green_result(report, monkeypatch, tmp_path, capsys):
    import os
    import tempfile

    monkeypatch.setenv("CLUD_TOOL_OUTPUT_DIR", str(tmp_path))
    root = report.artifact_root()
    fd, name = tempfile.mkstemp(prefix="local-test-", suffix=".log", dir=root)
    with os.fdopen(fd, "wb") as stream:
        stream.write(b"test one ... ok\n")
    monkeypatch.setattr(
        report,
        "run_command",
        lambda *_: {
            "artifact": name,
            "state": "timeout",
            "exit_code": 0,
            "bytes": 16,
            "pipe_eof": False,
            "child_exited": True,
            "termination_requested": True,
        },
    )
    assert report.main(["--", "unused"]) == 124
    result = report.json.loads(capsys.readouterr().out.splitlines()[0])
    assert result["verdict"] == "incomplete"
    assert result["capture_state"] == "timeout"
    assert result["tree_cleanup"] == "unverified"


@pytest.mark.skipif(
    __import__("sys").platform in {"win32", "darwin"}, reason="ACL checks pending"
)
def test_missing_eof_cannot_claim_complete(report, monkeypatch, tmp_path, capsys):
    import os
    import tempfile

    monkeypatch.setenv("CLUD_TOOL_OUTPUT_DIR", str(tmp_path))
    root = report.artifact_root()
    fd, name = tempfile.mkstemp(prefix="local-test-", suffix=".log", dir=root)
    os.close(fd)
    monkeypatch.setattr(
        report,
        "run_command",
        lambda *_: {
            "artifact": name,
            "state": "complete",
            "exit_code": 0,
            "bytes": 0,
            "pipe_eof": False,
            "child_exited": True,
        },
    )
    assert report.main(["--", "unused"]) == 1
    assert "complete safe report" in capsys.readouterr().err


def test_individual_pass_is_not_proof_of_suite_success(report):
    raw = b"test x ... ok\nFAIL test_y\n"
    data = report.json.loads(report.summarize(io.BytesIO(raw), exit_code=0))
    assert data["verdict"] != "passed"
    assert any("FAIL test_y" in item for item in data["diagnostics"])


def test_individual_pass_without_summary_is_unknown(report):
    data = report.json.loads(report.summarize(io.BytesIO(b"test x ... ok\n"), exit_code=0))
    assert data["verdict"] == "unknown"


def test_contradictory_summary_does_not_claim_success(report):
    raw = b"=== 1 passed, 1 failed in 0.1s ===\n"
    data = report.json.loads(report.summarize(io.BytesIO(raw), exit_code=0))
    assert data["verdict"] == "anomalous"
    assert "1 failed" in data["diagnostics"][0]


def test_pass_chatter_does_not_diagnose_a_failure(report):
    raw = b"test x ... ok\nopaque crash\n"
    data = report.json.loads(report.summarize(io.BytesIO(raw), exit_code=1))
    assert data["verdict"] == "failed"
    assert data["diagnostic_state"] == "unknown"


@pytest.mark.skipif(__import__("sys").platform in {"win32", "darwin"}, reason="ACL checks pending")
def test_sigkill_is_preserved_by_tool_entrypoint(monkeypatch, tmp_path):
    import os
    import signal
    import sys

    from running_process import RunningProcess

    monkeypatch.setenv("CLUD_TOOL_OUTPUT_DIR", str(tmp_path))
    result = RunningProcess.run(
        [
            sys.executable,
            str(TOOL),
            "--",
            sys.executable,
            "-c",
            "import os, signal; os.kill(os.getpid(), signal.SIGKILL)",
        ],
        capture_output=True,
        env=os.environ.copy(),
    )
    assert result.returncode == -signal.SIGKILL


@pytest.mark.skipif(
    __import__("sys").platform in {"win32", "darwin"}, reason="ACL checks pending"
)
def test_real_unittest_subtests_group_failures(report, monkeypatch, tmp_path, capsys):
    import json
    import shlex
    import sys

    monkeypatch.setenv("CLUD_TOOL_OUTPUT_DIR", str(tmp_path))
    program = """import unittest
class Example(unittest.TestCase):
    def test_cases(self):
        for i in range(300):
            with self.subTest(i=i):
                self.assertEqual(i, -1, 'actionable assertion')
unittest.main()
"""
    assert report.main(["--", sys.executable, "-c", program]) == 1
    output = capsys.readouterr().out
    lines = output.splitlines()
    data = json.loads(lines[0])
    artifact = Path(shlex.split(lines[1].removeprefix("artifact: "))[0])
    assert data["verdict"] == "failed"
    assert any(group["count"] == 300 for group in data["failure_groups"])
    assert any("AssertionError" in line for line in data["diagnostics"])
    assert len(output.encode()) * 10 < artifact.stat().st_size


def test_truncated_success_output_is_not_reported_as_passed(report):
    raw = b"x" * 9000 + b"\ntest result: ok. 1 passed\n"
    result = report.json.loads(report.summarize(io.BytesIO(raw), exit_code=0))
    assert result["verdict"] == "unknown"
    assert result["input_truncated"] is True
    assert result["diagnostic_state"] == "unknown"


@pytest.mark.skipif(
    __import__("sys").platform in {"win32", "darwin"}, reason="ACL checks pending"
)
def test_small_budget_with_long_path_preserves_failure(report, monkeypatch, tmp_path, capsys):
    import json
    import sys

    # Root plus the tool's private directory remains under the native 200-byte cap.
    base = tmp_path / ("x" * max(1, 165 - len(str(tmp_path))))
    monkeypatch.setenv("CLUD_TOOL_OUTPUT_DIR", str(base))
    status = report.main(["--limit", "512", "--", sys.executable, "-c", "import sys; sys.exit(7)"])
    output = capsys.readouterr().out
    result = json.loads(output.splitlines()[0])
    assert status == 7
    assert result["verdict"] == "failed"
    assert "artifact: " in output
    assert len(output.encode()) <= 512


@pytest.mark.skipif(
    __import__("sys").platform in {"win32", "darwin"}, reason="ACL checks pending"
)
def test_small_budget_with_many_diagnostics_keeps_result(report, monkeypatch, tmp_path, capsys):
    import json
    import sys

    monkeypatch.setenv("CLUD_TOOL_OUTPUT_DIR", str(tmp_path))
    body = "FAIL: test_" + "x" * 100 + "\nwarning: " + "w" * 100 + "\nerror: " + "e" * 100 + "\n"
    program = "import sys; sys.stderr.write(" + repr(body) + "); sys.exit(7)"
    status = report.main(["--limit", "512", "--", sys.executable, "-c", program])
    output = capsys.readouterr().out
    assert status == 7
    assert json.loads(output.splitlines()[0])["verdict"] == "failed"
    assert "artifact: " in output
    assert len(output.encode()) <= 512


def test_macos_fails_closed_before_command(report, monkeypatch, tmp_path, capsys):
    calls = []
    monkeypatch.setattr(report.sys, "platform", "darwin")
    monkeypatch.setenv("CLUD_TOOL_OUTPUT_DIR", str(tmp_path / "not-created"))
    monkeypatch.setattr(report, "run_command", lambda *_args: calls.append(True))
    assert report.main(["--", "irrelevant"]) == 1
    assert not calls
    assert "not-created" not in capsys.readouterr().err
    assert not (tmp_path / "not-created").exists()


@pytest.mark.skipif(
    __import__("sys").platform in {"win32", "darwin"}, reason="ACL checks pending"
)
def test_redirected_stdin_reaches_test_command(monkeypatch, tmp_path):
    import json
    import sys

    from running_process import RunningProcess

    monkeypatch.setenv("CLUD_TOOL_OUTPUT_DIR", str(tmp_path))
    program = (
        "import sys; data=sys.stdin.buffer.read(); "
        "sys.stdout.buffer.write(b'error: ' + data); sys.exit(7)"
    )
    result = RunningProcess.run(
        [sys.executable, str(TOOL), "--", sys.executable, "-c", program],
        input=b"redirected-input",
        capture_output=True,
        text=False,
        timeout=10,
    )
    assert result.returncode == 7
    summary = json.loads(result.stdout.splitlines()[0])
    assert any("redirected-input" in line for line in summary["diagnostics"])

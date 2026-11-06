"""Unit tests for what `ci/run_bundle.py` leaves behind when a CI lane fails.

Issue #994: the Windows lane exited 1 with no `FAILED` line and no pytest
summary -- "the process died rather than a test failing" -- and the log said
only `Process completed with exit code 1`. A reader cannot tell a crash from a
hang from an ordinary failure, so every occurrence costs a fresh diagnosis.

An exit code carries more than that if it is read. Three records under the
uploaded `logs/*` back it up: the teed stdout log (#1168), pytest's JUnit XML,
and a flushed test-boundary journal that survives an abrupt pytest exit (#1178).
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

from ci import run_bundle
from ci.run_bundle import (
    _pytest_ok,
    describe_pytest_exit,
    pytest_junit_path,
    pytest_log_path,
    pytest_progress_path,
    run_pytest,
    run_streamed,
)


def test_pytest_own_codes_are_named() -> None:
    """0-5 are pytest's; say which is which rather than echoing the number."""
    assert "all tests passed" in describe_pytest_exit(0)
    assert "abrupt exit" in describe_pytest_exit(1)
    assert "interrupted" in describe_pytest_exit(2)
    assert "internal error" in describe_pytest_exit(3)
    assert "usage error" in describe_pytest_exit(4)
    assert "no tests collected" in describe_pytest_exit(5)


def test_a_posix_signal_is_reported_as_a_crash() -> None:
    """A negative return is a signal, and the missing summary is then expected.

    This is the distinction #994 asks for: a segfault and a failed assertion
    both surface as a non-zero exit, and only one of them means the absent
    summary is a clue worth chasing."""
    described = describe_pytest_exit(-11)
    assert "SIGSEGV" in described
    assert "crashed" in described

    # Named from a fixed table, not the host's `signal.Signals`: Windows has
    # no SIGKILL, so reading a Linux lane's log there would otherwise render
    # the most common CI kill as a bare "signal 9". Caught by the Windows
    # lane doing exactly that.
    assert "SIGKILL" in describe_pytest_exit(-9)
    assert "SIGTERM" in describe_pytest_exit(-15)


def test_a_windows_ntstatus_is_reported_as_a_crash() -> None:
    """Windows reports a crash as a large unsigned code, not a signal.

    A POSIX-shaped `returncode < 0` check sees nothing wrong with
    0xC0000005, which is exactly the platform the issue was filed about."""
    described = describe_pytest_exit(0xC0000005)
    assert "EXCEPTION_ACCESS_VIOLATION" in described
    assert "not a test result" in described

    assert "STATUS_HEAP_CORRUPTION" in describe_pytest_exit(0xC0000374)


def test_an_undocumented_code_says_it_did_not_come_from_pytest() -> None:
    """The most useful thing to say about exit 7 is that pytest cannot produce
    it, so the cause is upstream of pytest's own decision-making."""
    described = describe_pytest_exit(7)
    assert "not one of pytest's documented codes" in described


def test_success_codes_are_unchanged() -> None:
    """The diagnosis is reporting only; it must not redefine success.

    `5` (no tests collected) stays OK because a lane that selects an empty
    marker set is not a failure -- that predates this change and is relied on."""
    assert _pytest_ok(0)
    assert _pytest_ok(5)
    assert not _pytest_ok(1)
    assert not _pytest_ok(-11)


def test_run_streamed_tees_output_to_the_log_as_it_arrives(tmp_path: Path, capsys) -> None:
    """#1168: the cancelled job's step log was empty, so the tee is the record.

    Both streams land in the file (each in its own order -- running-process
    reads them on separate threads, so cross-stream order is not pinned), the
    file is flushed per line so a process killed later still leaves the
    earlier lines, the echo still reaches the step log, and the exit code is
    the child's."""
    log = tmp_path / "nested" / "pytest-unit.log"
    script = (
        "import sys; print('collected 3 items'); "
        "print('E   AssertionError', file=sys.stderr); print('1 failed'); sys.exit(1)"
    )
    rc = run_streamed([sys.executable, "-c", script], {}, log)
    assert rc == 1
    body = log.read_text(encoding="utf-8")
    lines = body.splitlines()
    assert "collected 3 items" in lines
    assert "E   AssertionError" in lines
    assert "1 failed" in lines
    assert lines.index("collected 3 items") < lines.index("1 failed")
    assert len(lines) == 3, body
    assert "collected 3 items" in capsys.readouterr().out


def test_run_streamed_forces_an_unbuffered_child(tmp_path: Path) -> None:
    """A wedged pytest with a block-buffered stdout tees nothing useful;
    the child must be unbuffered so each `-v` line reaches the file."""
    log = tmp_path / "pytest.log"
    rc = run_streamed(
        [sys.executable, "-c", "import os; print(os.environ.get('PYTHONUNBUFFERED'))"], {}, log
    )
    assert rc == 0
    assert log.read_text(encoding="utf-8") == "1\n"


def test_pytest_log_path_is_under_the_uploaded_logs_dir() -> None:
    """`_run-tests.yml` uploads `logs/*`; the tee has to land there."""
    assert pytest_log_path("integration").parent.name == "logs"
    assert pytest_log_path("unit").name == "pytest-unit.log"


def test_pytest_junit_path_is_under_the_uploaded_logs_dir() -> None:
    """#1178: `_run-tests.yml` already uploads `logs/*`, so a pytest-written
    junit report needs no workflow change to reach the artifact."""
    assert pytest_junit_path("integration").parent.name == "logs"
    assert pytest_junit_path("unit").name == "pytest-unit.xml"


def test_run_pytest_records_junit_and_progress(monkeypatch) -> None:
    """Ordinary failures get JUnit; abrupt exits leave the progress journal.

    The flags precede caller extras so a caller can override the JUnit options.
    """
    captured: list[tuple[list[str], dict[str, str], Path]] = []

    def fake(argv: list[str], env: dict[str, str], log_path: Path) -> int:
        captured.append((list(argv), env, log_path))
        return 0

    monkeypatch.setattr(run_bundle, "run_streamed", fake)

    env = {"CLUD_INTEGRATION_TESTS": "1"}
    assert run_pytest("integration", env, ["-v"], suite="integration") == 0
    argv, passed_env, log_path = captured[0]

    junit_flag = f"--junitxml={pytest_junit_path('integration')}"
    assert junit_flag in argv
    assert argv[argv.index("-p") + 1] == "ci.pytest_progress"
    assert argv[argv.index("-o") + 1] == "junit_logging=all"

    assert argv.index(junit_flag) < argv.index("-v")
    assert argv.index("-o") < argv.index("-v")
    assert argv.index("--durations=25") < argv.index("-v")

    assert argv[:5] == [sys.executable, "-m", "pytest", "-m", "integration"]
    assert log_path == pytest_log_path("integration")
    assert passed_env == {
        **env,
        "CLUD_PYTEST_PROGRESS_LOG": str(pytest_progress_path("integration")),
    }


def test_abrupt_pytest_exit_leaves_the_active_test_in_progress_log(
    tmp_path: Path, monkeypatch
) -> None:
    """An os._exit during a test leaves no pytest summary or JUnit sessionfinish.

    The uploaded progress record must still identify the test that was active.
    """
    monkeypatch.setattr(run_bundle, "LOG_DIR", tmp_path / "logs")
    victim = tmp_path / "test_abrupt_exit.py"
    victim.write_text(
        "import os\n\ndef test_abrupt_exit():\n    os._exit(1)\n",
        encoding="utf-8",
    )

    # This child exercises only our explicit plugin. Autoloading unrelated
    # plugins can fail during Windows runner teardown before the test starts.
    assert run_pytest(
        "not integration",
        {"PYTEST_DISABLE_PLUGIN_AUTOLOAD": "1"},
        ["-v", str(victim)],
        suite="unit",
    ) == 1
    entries = [
        json.loads(line)
        for line in pytest_progress_path("unit").read_text(encoding="utf-8").splitlines()
    ]
    assert [entry["event"] for entry in entries] == ["start"]
    # pytest may omit the external temp file's path from a Windows node ID;
    # the test function remains the stable active-test identifier.
    assert entries[0]["nodeid"].endswith("::test_abrupt_exit")
    assert "short test summary info" not in pytest_log_path("unit").read_text(encoding="utf-8")


def test_progress_journal_survives_the_repo_conftest_scrub(tmp_path: Path, monkeypatch) -> None:
    """#1625: `tests/conftest.py` scrubs `CLUD_*` from `os.environ` at import.

    The journal must still be written by the pytest session itself, while the
    tests, and every child process they spawn (children inherit `os.environ`),
    must not see the variable. `-p tests.conftest` loads the repo conftest,
    scrub included, for a test file that lives outside `tests/`.
    """
    monkeypatch.setattr(run_bundle, "LOG_DIR", tmp_path / "logs")
    victim = tmp_path / "test_scrubbed_child_env.py"
    victim.write_text(
        "import os\n\n"
        "def test_child_env_is_scrubbed():\n"
        "    assert 'CLUD_PYTEST_PROGRESS_LOG' not in os.environ\n",
        encoding="utf-8",
    )

    # `run_pytest` replaces the child environment wholesale, and the repo
    # conftest spawns `git` at session start. Without the parent's PATH and
    # SYSTEMROOT, Windows cannot find it ("program not found", #1654).
    assert (
        run_pytest(
            "not integration",
            {**os.environ, "PYTEST_DISABLE_PLUGIN_AUTOLOAD": "1"},
            ["-v", "-p", "tests.conftest", str(victim)],
            suite="unit",
        )
        == 0
    )
    journal = pytest_progress_path("unit")
    assert journal.is_file(), "the repo conftest scrub swallowed the journal path (#1625)"
    entries = [json.loads(line) for line in journal.read_text(encoding="utf-8").splitlines()]
    assert [entry["event"] for entry in entries] == ["start", "finish"]
    assert all(entry["nodeid"].endswith("::test_child_env_is_scrubbed") for entry in entries)


def test_required_harness_rejects_empty_and_wholly_skipped_results(tmp_path: Path) -> None:
    report = tmp_path / "harness.xml"
    assert not run_bundle.required_harness_result(5, report)
    assert not run_bundle.required_harness_result(0, report)
    for content in [
        "invalid XML", "<testsuites/>",
        "<testsuite><testcase><skipped/></testcase></testsuite>",
    ]:
        report.write_text(content, encoding="utf-8")
        assert not run_bundle.required_harness_result(0, report)
    report.write_text("<testsuite><testcase/><testcase><skipped/></testcase></testsuite>")
    assert run_bundle.required_harness_result(0, report)
    assert not run_bundle.required_harness_result(1, report)
    assert run_bundle.report_pytest_exit(5), "optional empty shards retain their contract"


def test_ci_marker_selects_real_harness_tests_without_running_them() -> None:
    from tests.process import run as run_process

    result = run_process(
        [
            sys.executable, "-m", "pytest", "--collect-only", "-q", "-m", "real_claude",
            "tests/harness/test_smoke.py", "tests/test_run_bundle_exit.py",
        ],
        cwd=Path(__file__).resolve().parents[1],
        capture_output=True,
        timeout=30,
        check=False,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    output = result.stdout.decode()
    assert "test_scripted_bash_call_runs_and_is_recorded" in output
    assert "test_command_guard_denies_are_visible_to_the_script" in output
    assert "test_success_codes_are_unchanged" not in output

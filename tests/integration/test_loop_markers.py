"""Integration tests for the `clud loop` DONE/BLOCKED marker-file contract."""

from __future__ import annotations

from pathlib import Path

import pytest

from tests import process

pytestmark = pytest.mark.integration

# Scaled to the work: the heaviest tests here run `--loop-count 10`, ten full
# clud launches through the mock agent under one budget. 30s was the
# single-launch number from `test_mock_agents` (#994) and left 3s per
# iteration -- tighter than the budget that already failed at three
# iterations on a loaded Windows runner (see `test_workspace_trust_notice`).
#
# Capped at 75 rather than scaled the whole way: `pyproject.toml` sets
# pytest-timeout's per-test `timeout = 90`, so anything at or above that is
# unreachable -- the test is killed before the budget expires. Ten iterations
# at the ~12s/iteration this lane showed under load would exceed 90s outright,
# so if that ever becomes the normal case the per-test ceiling is the thing to
# revisit, not this number.
_TIMEOUT = 75


def _run(
    clud: Path,
    *args: str,
    env: dict[str, str],
    cwd: Path,
) -> process.CompletedProcess[str]:
    return process.run(
        [str(clud), *args],
        capture_output=True,
        text=True,
        timeout=_TIMEOUT,
        env=env,
        cwd=cwd,
    )


def _assert_rc(result: process.CompletedProcess[str], expected: int) -> None:
    """Assert the exit code, reporting what actually happened.

    Issue #594: `assert result.returncode == N, f"stderr: {result.stderr}"`
    omits the two facts a triager needs first — the code that *was* returned,
    and stdout. A real failure of this suite surfaced with a stderr consisting
    only of `Warning: .clud/loop was added to .gitignore`, which reads like a
    cause and is merely a notice; the unexpected exit code itself was never
    printed, so the message pointed away from the failure.
    """
    assert result.returncode == expected, (
        f"expected exit {expected}, got {result.returncode}\n"
        f"--- stderr ---\n{result.stderr}\n"
        f"--- stdout (tail) ---\n{result.stdout[-2000:]}"
    )


def _marker_dir(cwd: Path) -> Path:
    d = cwd / ".clud" / "loop"
    d.mkdir(parents=True, exist_ok=True)
    return d


def test_loop_stops_on_done_marker(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """Agent writes DONE at iteration 3 of 10 → loop exits 0 at iteration 3."""
    loop_dir = _marker_dir(tmp_path)
    done = loop_dir / "DONE"

    result = _run(
        clud_binary,
        "loop",
        "--loop-count",
        "10",
        "resolve the task",
        "--",
        "--mock-write-done",
        str(done),
        "--mock-write-done-body",
        "task resolved",
        "--mock-write-marker-on-iter",
        "3",
        env=mock_env,
        cwd=tmp_path,
    )
    _assert_rc(result, 0)
    assert "iteration 3" in result.stderr
    assert "DONE" in result.stderr
    assert done.is_file()
    assert done.read_text().strip() == "task resolved"


def test_loop_stops_on_blocked_marker(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """Agent writes BLOCKED at iteration 2 → loop exits 3 with reason."""
    loop_dir = _marker_dir(tmp_path)
    blocked = loop_dir / "BLOCKED"

    result = _run(
        clud_binary,
        "loop",
        "--loop-count",
        "10",
        "task",
        "--",
        "--mock-write-blocked",
        str(blocked),
        "--mock-write-blocked-body",
        "missing credentials",
        "--mock-write-marker-on-iter",
        "2",
        env=mock_env,
        cwd=tmp_path,
    )
    _assert_rc(result, 3)
    assert "BLOCKED" in result.stderr
    assert "missing credentials" in result.stderr


def test_loop_no_markers_exhausts_iterations(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """Agent never writes a marker → loop runs all iterations, exits 2."""
    result = _run(
        clud_binary,
        "loop",
        "--loop-count",
        "3",
        "task",
        env=mock_env,
        cwd=tmp_path,
    )
    _assert_rc(result, 2)
    assert "did not converge" in result.stderr
    assert "iteration 1" in result.stderr
    assert "iteration 3" in result.stderr


def test_loop_no_done_flag_keeps_old_semantics(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """With --no-done the loop runs all iterations and exits 0.

    This preserves pre-marker behavior for scripts that don't want the
    DONE-marker contract injected.
    """
    result = _run(
        clud_binary,
        "loop",
        "--loop-count",
        "2",
        "--no-done",
        "task",
        env=mock_env,
        cwd=tmp_path,
    )
    _assert_rc(result, 0)
    # No "did not converge" message — the contract isn't active.
    assert "did not converge" not in result.stderr


def test_loop_clears_stale_done_marker(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """A DONE marker left over from a prior run must be cleared at start.

    Otherwise the loop would short-circuit on iteration 1 without doing work.
    """
    loop_dir = _marker_dir(tmp_path)
    done = loop_dir / "DONE"
    done.write_text("stale")

    # Agent is instructed to never re-write DONE. If the old stale marker
    # weren't cleared, the loop would exit after iteration 1. Expect 2 (not
    # converged) since loop-count=2 and no new DONE is written.
    result = _run(
        clud_binary,
        "loop",
        "--loop-count",
        "2",
        "task",
        env=mock_env,
        cwd=tmp_path,
    )
    _assert_rc(result, 2)
    assert "did not converge" in result.stderr


def test_loop_dry_run_includes_loop_markers(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """--dry-run output reports the active DONE/BLOCKED marker paths."""
    import json

    result = _run(
        clud_binary,
        "--dry-run",
        "loop",
        "task",
        env=mock_env,
        cwd=tmp_path,
    )
    _assert_rc(result, 0)
    data = json.loads(result.stdout)
    assert data["loop_markers"] is not None
    assert data["loop_markers"]["done_path"].replace("\\", "/").endswith(".clud/loop/DONE")
    assert data["loop_markers"]["blocked_path"].replace("\\", "/").endswith(".clud/loop/BLOCKED")


def test_loop_emits_clud_loop_artifacts(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """A `clud loop` run produces info.json + log.txt under .clud/loop/ and
    auto-injects `.clud/loop` into a pre-existing .gitignore. Issue #96.

    This is the integration counterpart of the Rust unit tests in
    `loop_artifacts::tests` — it verifies the bookkeeping is actually
    wired into the running runner, not just exposed as a library API.
    """
    import json

    # Seed a .gitignore that does NOT yet cover .clud/loop so we can also
    # assert the auto-injection ran end-to-end.
    gitignore = tmp_path / ".gitignore"
    gitignore.write_text("target/\nfoo.txt\n", encoding="utf-8")

    # Need a .git so git_root_from() resolves tmp_path as the root; the
    # implementation uses that to anchor .clud/loop and .gitignore.
    (tmp_path / ".git").mkdir()

    result = _run(
        clud_binary,
        "loop",
        "--loop-count",
        "2",
        "task body",
        env=mock_env,
        cwd=tmp_path,
    )
    # No marker is written by the mock agent → loop exhausts and exits 2.
    _assert_rc(result, 2)

    # info.json exists, parses, has at least one iteration recorded.
    info_path = tmp_path / ".clud" / "loop" / "info.json"
    assert info_path.is_file(), f"missing info.json under {tmp_path}"
    info = json.loads(info_path.read_text(encoding="utf-8"))
    assert isinstance(info, dict)
    assert info.get("total_iterations") == 2
    assert info.get("completed") is True
    iterations = info.get("iterations") or []
    assert len(iterations) >= 1, f"expected at least one iteration; got: {iterations!r}"

    # log.txt exists and contains iteration headers / footers.
    log_path = tmp_path / ".clud" / "loop" / "log.txt"
    assert log_path.is_file(), f"missing log.txt under {tmp_path}"
    log = log_path.read_text(encoding="utf-8")
    assert log.strip(), "log.txt is empty"
    assert "iteration 1 start" in log
    assert "loop start" in log

    # .gitignore got `.clud/loop` appended.
    gi = gitignore.read_text(encoding="utf-8")
    assert ".clud/loop" in gi, f"gitignore missing .clud/loop entry; got: {gi!r}"


def test_codex_loop_stops_on_done_marker(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """Codex loop honors the DONE marker contract just like claude."""
    loop_dir = _marker_dir(tmp_path)
    done = loop_dir / "DONE"

    result = _run(
        clud_binary,
        "--codex",
        "loop",
        "--loop-count",
        "10",
        "resolve the task",
        "--",
        "--mock-write-done",
        str(done),
        "--mock-write-done-body",
        "codex task resolved",
        "--mock-write-marker-on-iter",
        "2",
        env=mock_env,
        cwd=tmp_path,
    )
    _assert_rc(result, 0)
    assert "iteration 2" in result.stderr
    assert "DONE" in result.stderr
    assert done.is_file()
    assert done.read_text().strip() == "codex task resolved"


def test_loop_message_without_task_queues_and_exits(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """`clud loop --message X` with no task queues X in the inbox and exits 0 (#2471)."""
    result = _run(
        clud_binary,
        "loop",
        "--message",
        "focus on the failing test",
        env=mock_env,
        cwd=tmp_path,
    )
    _assert_rc(result, 0)
    inbox = tmp_path / ".clud" / "loop" / "inbox"
    queued = sorted(inbox.glob("*.md"))
    assert len(queued) == 1, list(inbox.iterdir())
    assert queued[0].read_text() == "focus on the failing test"
    assert "queued message" in result.stderr


def test_loop_message_is_delivered_and_root_summary_written(
    clud_binary: Path, mock_env: dict[str, str], tmp_path: Path
) -> None:
    """A queued message reaches NEXT_MESSAGE.md and LOOP_DONE.md lands at the root (#2471)."""
    loop_dir = _marker_dir(tmp_path)
    done = loop_dir / "DONE"
    result = _run(
        clud_binary,
        "loop",
        "--loop-count",
        "2",
        "--message",
        "use the new API",
        "resolve the task",
        "--",
        "--mock-write-done",
        str(done),
        "--mock-write-done-body",
        "ok",
        "--mock-write-marker-on-iter",
        "1",
        env=mock_env,
        cwd=tmp_path,
    )
    _assert_rc(result, 0)
    next_message = loop_dir / "NEXT_MESSAGE.md"
    assert "use the new API" in next_message.read_text()
    assert len(list((loop_dir / "inbox" / "delivered").glob("*.md"))) == 1
    summary = (tmp_path / "LOOP_DONE.md").read_text()
    assert "# LOOP_DONE" in summary
    assert ".clud/loop/info.json" in summary

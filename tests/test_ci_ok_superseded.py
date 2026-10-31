"""A superseded (cancelled) CI run must not publish a failing CI OK (#2273)."""

from __future__ import annotations

import re
from pathlib import Path

WORKFLOWS = Path(__file__).resolve().parent.parent / ".github" / "workflows"


def _ci_text() -> str:
    return (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")


def _job_block(job: str) -> str:
    match = re.search(rf"(?ms)^  {job}:\n(.*?)(?=^  [a-z][a-z0-9-]*:\n|\Z)", _ci_text())
    assert match, f"job {job!r} not found in ci.yml"
    return match.group(1)


def test_ci_ok_still_runs_in_a_cancelled_run() -> None:
    """#2273: a skipped required check reads as success, so CI OK must run."""
    block = _job_block("ci-ok")
    assert "name: CI OK" in block
    assert "    if: always()\n" in block
    assert "!cancelled()" not in block.split("steps:", 1)[0]


def test_only_a_superseded_run_skips_verification() -> None:
    """#2273: a superseded run passes; a hand-cancelled one still fails."""
    steps = _job_block("ci-ok").split("steps:", 1)[1]
    detect = steps.split("- name: Detect superseded run", 1)[1].split("- name:", 1)[0]
    assert "contains(toJSON(needs.*.result), 'cancelled')" in detect
    assert "head_sha=$HEAD_SHA" in detect
    assert "select(.id > $GITHUB_RUN_ID)" in detect
    assert 'echo "superseded=true"' in detect
    check = steps.split("- name: Check results", 1)[1]
    assert "if: steps.superseded.outputs.superseded != 'true'" in check


def test_superseded_pr_runs_are_still_cancelled() -> None:
    """#2273: the guarded scenario is concurrency cancelling a superseded run."""
    header = _ci_text().split("\njobs:", 1)[0]
    assert "concurrency:" in header
    assert "cancel-in-progress:" in header


def test_ci_ok_still_requires_windows_coverage() -> None:
    """#2273: the fix must keep required Windows lanes in the gate."""
    block = _job_block("ci-ok")
    needs = block.split("needs:", 1)[1].split("runs-on:", 1)[0]
    assert "- build-windows-x64" in needs
    assert "- test-windows-x64" in needs

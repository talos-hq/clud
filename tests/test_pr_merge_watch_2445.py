"""pr_merge_watch: a failing aggregate gate must lead to its real failures (#2445).

A branch-protected `CI OK` job fails only because the jobs it `needs` failed;
its log holds nothing but `##[error]Process completed with exit code 1.`
Reporting that as the first error sent the agent back to
`gh run view --log-failed` for kilobytes of setup noise: one more tool call
that resends the whole context.
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "crates" / "clud-bin" / "assets" / "tools" / "github" / "pr_merge_watch.py"


@pytest.fixture(scope="module")
def watcher():
    spec = importlib.util.spec_from_file_location("pr_merge_watch_2445", SCRIPT)
    assert spec
    assert spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


AGGREGATE_LOG = """\
Run python3 ci/ci_ok.py
some job failed
##[error]Process completed with exit code 1.
"""


def test_generic_exit_annotation_is_not_a_first_error(watcher) -> None:
    assert watcher.first_error_line(AGGREGATE_LOG) == ""


def test_generic_exit_annotation_yields_to_a_real_error(watcher) -> None:
    log = (
        "##[error]Process completed with exit code 101.\n"
        "error[E0554]: `#![feature]` may not be used\n"
    )
    assert watcher.first_error_line(log) == "error[E0554]: `#![feature]` may not be used"


def test_uninformative_gate_failure_diagnoses_advisory_failures(watcher) -> None:
    primary = watcher.FailureReport(
        watcher.CheckRow(name="CI OK", bucket="fail", state="FAILURE"), "1", "", None
    )
    advisory = ["Dylint (host)", "Test unit (rust)", "Test unit (py1)", "Test unit (py2)"]
    built = []

    def build(name):
        built.append(name)
        return f"report:{name}"

    causes = watcher.root_cause_reports(primary, advisory, build)
    assert causes == [f"report:{name}" for name in advisory[: watcher.ROOT_CAUSE_REPORTS]]
    assert built == advisory[: watcher.ROOT_CAUSE_REPORTS]


def test_informative_failure_needs_no_root_causes(watcher) -> None:
    primary = watcher.FailureReport(
        watcher.CheckRow(name="Test", bucket="fail", state="FAILURE"), "1", "FAILED t::x", None
    )
    assert watcher.root_cause_reports(primary, ["other"], lambda name: name) == []

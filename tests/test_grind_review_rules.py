"""Guard the /grind review gate, parking and retry rules in the bundled assets.

#1424: a reviewer never rejects because nothing has been run; it lists
`must_verify` for the integrator, and a rejected sequential goal is parked.
#1425: the integrator reads a failure before retrying and never loops a suite.
The end-to-end behavior is in `tests/harness/test_grind_review_gate.py`.
"""

from __future__ import annotations

import re
from pathlib import Path

ASSETS = Path(__file__).resolve().parents[1] / "crates/clud-bin/assets"


def _flat(text: str) -> str:
    return " ".join(text.split())


def _skill(name: str) -> str:
    return _flat((ASSETS / "skills" / name / "SKILL.md").read_text(encoding="utf-8"))


def _agent(name: str) -> str:
    return _flat((ASSETS / "agents" / f"{name}.md").read_text(encoding="utf-8"))


def _workflow() -> str:
    return (ASSETS / "workflows" / "grind-run.js").read_text(encoding="utf-8")


def _not_run() -> re.Pattern[str]:
    """The workflow's NOT_RUN override pattern, compiled the same way."""
    js = _workflow()
    block = js[js.index("const NOT_RUN = new RegExp([") :]
    block = block[: block.index("].join('|'), 'i')")]
    parts = re.findall(r"String\.raw`([^`]*)`", block)
    assert parts, "NOT_RUN has no alternatives"
    return re.compile("|".join(parts), re.IGNORECASE)


# ---- #1424 -------------------------------------------------------------------


def test_review_never_rejects_for_unrun_checks() -> None:
    text = _skill("grind-review")
    assert '"Not yet run" is never a reason to reject.' in text
    assert "must_verify" in text
    assert "Approve on reading" in text
    # The old rule is what made reviewers reject for "nothing has been run".
    assert "cannot be made correct without running something" not in text


def test_reviewer_agent_puts_unrun_checks_in_must_verify() -> None:
    assert "must_verify" in _agent("grind-reviewer")


def test_integrator_runs_must_verify() -> None:
    text = _skill("grind-integrate")
    assert "**Run must_verify.**" in text
    assert "run every one before pushing" in text


def test_workflow_hands_must_verify_to_the_integrator() -> None:
    js = _workflow()
    assert "must_verify: { type: 'array'" in js
    assert "withMustVerify(planned, rv)" in js
    assert "# reviewer must_verify" in js


def test_not_run_override_matches_only_unrun_check_rejections() -> None:
    pattern = _not_run()
    for summary in (
        "Nothing has been run yet... My role can't run tests or lint",
        "Nothing has been run yet.",
        "The tests have not been run.",
        "Tests haven't been executed yet.",
        "Not verified yet: my role cannot run the suite.",
        "I cannot run tests, so I can't confirm this.",
    ):
        assert pattern.search(summary), summary
    for summary in (
        "The retry path drops the error; the change is wrong.",
        "The new test is never registered, so it cannot fail.",
        "Handler returns early and the run loop never ends.",
    ):
        assert not pattern.search(summary), summary


def test_integrator_parks_a_failed_sequential_goal() -> None:
    text = _skill("grind-integrate")
    assert "**Park (sequential mode, when the prompt says `PARK goal`).**" in text
    assert "`wip/grind-<goal>` stays local: never push it." in text
    assert "`git switch --detach origin/<base>`" in text
    assert "Never `git reset --hard`, `git clean`" in text
    assert "never `git add -A`" in text
    assert "The user's pre-run state" in text


def test_workflow_parks_and_blocks_dependents() -> None:
    js = _workflow()
    assert "PARK goal ${g.id}" in js
    assert "wip/grind-${slug(g.id)" in js
    assert "Preflight (the user's pre-run state; never park, reset or discard it)" in js
    assert "was rejected" in js
    assert "blocked: dependency ${dep}" in js
    # A dependency that already settled blocks before any worker writes.
    attempt = js[js.index("const attemptStagedGoal") :]
    assert attempt.index("settledBlock(p)") < attempt.index("await work(state.p, g)")


def test_finish_keeps_park_branches() -> None:
    assert "Never delete a `wip/grind-<goal>` park branch" in _skill("grind")


# ---- #1425 -------------------------------------------------------------------


def test_integrator_reads_failures_before_retrying() -> None:
    text = _skill("grind-integrate")
    for rule in (
        "**Read before retrying.**",
        "read the failing test names and their errors first",
        "**Retry only infrastructure errors.**",
        "`SESSION relay closed before Exit`",
        "at most twice",
        "Never wrap a full suite in a retry loop",
        "**Same failure twice is real.**",
        "fails on untouched `origin/<main>`",
        "**Wait by condition.**",
        "sleep blocks",
    ):
        assert rule in text, rule


def test_integrator_agent_carries_the_retry_rule() -> None:
    text = _agent("grind-integrator")
    assert "read the failing tests and errors before anything else" in text
    assert "never in a loop" in text

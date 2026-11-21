"""`./push`: attest the committed branch, push it, open its PR (#2873).

`/grind`'s gate detection (`grind/detect_gate.py`) treats a repository's
`./push` as its whole local gate, so these tests pin what clud's runs and
what it refuses.
"""

from __future__ import annotations

import sys
from collections.abc import Sequence

import pytest

from ci import push


def _argv(steps: list[push.Step]) -> list[str]:
    return [" ".join(s.argv) for s in steps]


def test_default_plan_attests_host_lanes_pushes_and_opens_a_pr() -> None:
    steps = push.plan("feat/x", python="py", cross=False, test=False, pr=True)
    assert [s.name for s in steps] == ["attest", "push", "pr"]
    assert _argv(steps)[0] == "py -m ci.attest clippy dylint"
    # prooft amends HEAD's message, so the push needs a lease.
    assert _argv(steps)[1] == "git push --force-with-lease -u origin HEAD:refs/heads/feat/x"
    assert _argv(steps)[2] == "gh pr create --fill --base main --head feat/x"


def test_cross_and_test_flags_extend_the_plan() -> None:
    steps = push.plan("feat/x", python="py", cross=True, test=True, pr=False, base="dev")
    assert [s.name for s in steps] == ["test", "attest", "push"]
    assert _argv(steps)[0] == "bash test --host"
    for lane in push.CROSS_LANES:
        assert lane in _argv(steps)[1]


@pytest.mark.parametrize(
    ("branch", "dirty", "needle"),
    [
        ("main", False, "refusing to push main"),
        ("master", False, "refusing to push master"),
        ("HEAD", False, "detached"),
        ("", False, "detached"),
        ("feat/x", True, "dirty tree"),
    ],
)
def test_refusals(branch: str, dirty: bool, needle: str) -> None:
    why = push.refusal(branch, dirty)
    assert why is not None
    assert needle in why


def test_a_feature_branch_with_a_clean_tree_is_allowed() -> None:
    assert push.refusal("feat/x", dirty=False) is None


def test_a_failing_step_stops_everything_after_it(monkeypatch: pytest.MonkeyPatch) -> None:
    answers = {("git", "rev-parse"): "feat/x", ("git", "status"): ""}
    monkeypatch.setattr(push, "_out", lambda argv: (0, answers.get(tuple(argv[:2]), "")))
    ran: list[str] = []

    def run(argv: Sequence[str]) -> int:
        ran.append(argv[2] if argv[0] == sys.executable else argv[0])
        return 1 if argv[0] == sys.executable else 0

    assert push.main([], run=run) == 1
    assert ran == ["ci.attest"]


def test_an_existing_pr_is_reused(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    url = "https://github.com/talos-hq/clud/pull/1"
    answers = {("git", "rev-parse"): "feat/x", ("git", "status"): "", ("gh", "pr"): url}
    monkeypatch.setattr(push, "_out", lambda argv: (0, answers.get(tuple(argv[:2]), "")))
    ran: list[str] = []
    assert push.main([], run=lambda argv: ran.append(argv[0]) or 0) == 0
    assert "gh" not in ran
    assert url in capsys.readouterr().out


def test_refused_branch_exits_3(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(push, "_out", lambda argv: (0, "main" if argv[1] == "rev-parse" else ""))
    assert push.main([], run=lambda argv: 0) == 3
